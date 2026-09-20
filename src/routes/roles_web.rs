// M5 identity (slice S4): the roles administration screen and the permission
// matrix. `/roles` list + create `<dialog>` + an edit dialog that loads, per
// role, the details form (name, description) AND the permission matrix — the
// 23 seeded codes grouped by module, each carrying its Spanish description the
// operator reads before ticking — on the exact customers/users pattern:
// Askama + HTMX, thin handlers with no SQL, collection endpoints with the id
// in the body (`/web/roles`, `/web/roles/edit`, `/web/roles/delete`,
// `/web/roles/matrix`), `#[serde(default)]` on `Form` fields, `HX-Trigger`
// events and the notice idiom (`data-action` feeding base.html's success box;
// refusals arrive as the JSON error the global htmx:responseError handler
// renders into the `#notice` box).
//
// Gating (deliberate, written into the spec): the catalog has no
// `identity.roles.read` — this slice does not invent a permission code (that
// would need a migration) — so EVERY surface here, reads included, declares
// `Require<IdentityRolesManage>`. The cost, stated in the spec: an operator
// who may only look at roles must hold the management permission.
//
// The protected role is presented as locked: no delete, no code rename, no
// matrix edit — the triggers enforce it, the markup never offers it, and the
// handlers refuse it with the reason in Spanish (its label and description
// stay editable, as the triggers allow). A matrix edit that would strip
// `identity.roles.manage` from a role the acting principal holds is refused
// by the service: the self-lockout the users screen already refuses for role
// sets. The matrix form's checkboxes repeat the `permission_ids` key, so that
// one handler reads the raw body through its own limit — the same raw-body
// parse pattern and 413-in-Spanish shape the roles assignment form ships.
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
use crate::models::{Permission, Role, RoleWithHolders};
use crate::routes::AppState;
use crate::security::authz::{IdentityRolesManage, Principal, Require};

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "roles.html")]
struct RolesTemplate {
    roles: Vec<RoleWithHolders>,
    nav_key: &'static str,
}

#[derive(Template)]
#[template(path = "partials/role_list.html")]
struct RoleListPartial {
    roles: Vec<RoleWithHolders>,
}

/// One permission row of the matrix, with the held flag precomputed (Askama
/// cannot diff two lists in a template).
struct MatrixPermission {
    permission: Permission,
    held: bool,
}

/// The catalog grouped by module — the matrix renders one section per module,
/// every row showing its code and its seeded Spanish description.
struct ModuleGroup {
    module: String,
    permissions: Vec<MatrixPermission>,
}

/// The edit dialog's fragment: the role's details form and, for an editable
/// role, the permission matrix. The protected role renders read-only: no
/// delete, no rename, no matrix edit — the interface must not offer what the
/// triggers refuse. `description_value` is the precomputed input value: the
/// description is optional and Askama cannot spell an empty string inside a
/// quoted attribute.
#[derive(Template)]
#[template(path = "partials/role_edit_form.html")]
struct RoleEditFormPartial {
    role: Role,
    description_value: String,
    groups: Vec<ModuleGroup>,
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

fn permissions_repo(state: &AppState) -> crate::repositories::SqlitePermissionRepository {
    crate::repositories::SqlitePermissionRepository::new(state.pool.clone())
}

/// Group the catalog by module, marking the role's held permissions. The
/// repository orders by (module, action, id), so the groups arrive in the
/// stable order the matrix renders.
fn module_groups(catalog: Vec<Permission>, held_ids: &[i64]) -> Vec<ModuleGroup> {
    let mut groups: Vec<ModuleGroup> = Vec::new();
    for permission in catalog {
        let held = held_ids.contains(&permission.id);
        match groups.last_mut() {
            Some(group) if group.module == permission.module => {
                group.permissions.push(MatrixPermission { permission, held });
            }
            _ => groups.push(ModuleGroup {
                module: permission.module.clone(),
                permissions: vec![MatrixPermission { permission, held }],
            }),
        }
    }
    groups
}

async fn list_response(state: &AppState) -> AppResult<Response> {
    let roles = state.identity_service.list_roles_with_holders().await?;
    Ok(Html(render(RoleListPartial { roles })?).into_response())
}

// ---------------------------------------------------------------------------
// Pages + fragments (every read gated `identity.roles.manage`)
// ---------------------------------------------------------------------------

async fn roles_page(
    State(state): State<AppState>,
    _: Require<IdentityRolesManage>,
) -> Result<Html<String>, AppError> {
    let roles = state.identity_service.list_roles_with_holders().await?;
    let tmpl = RolesTemplate {
        roles,
        nav_key: "roles",
    };
    Ok(Html(render(tmpl)?))
}

async fn web_role_list(
    State(state): State<AppState>,
    _: Require<IdentityRolesManage>,
) -> AppResult<Response> {
    list_response(&state).await
}

// ---------------------------------------------------------------------------
// Forms (HTMX, collection endpoints with the typed id in the body)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct CreateRoleForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct RoleIdForm {
    #[serde(default)]
    pub role_id: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct EditRoleForm {
    #[serde(default)]
    pub role_id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

async fn web_create_role(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityRolesManage>,
    Form(form): Form<CreateRoleForm>,
) -> AppResult<Response> {
    state
        .identity_service
        .create_role(
            &permissions_repo(&state),
            principal.user_id,
            &form.code,
            &form.name,
            &form.description,
        )
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "role-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/roles").into_response())
}

async fn web_edit_role(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityRolesManage>,
    Form(form): Form<EditRoleForm>,
) -> AppResult<Response> {
    state
        .identity_service
        .update_role(
            &permissions_repo(&state),
            principal.user_id,
            form.role_id,
            &form.name,
            &form.description,
        )
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "role-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/roles").into_response())
}

async fn web_delete_role(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityRolesManage>,
    Form(form): Form<RoleIdForm>,
) -> AppResult<Response> {
    state
        .identity_service
        .delete_role(&permissions_repo(&state), principal.user_id, form.role_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "role-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/roles").into_response())
}

/// The matrix replacement endpoint. The checkboxes repeat the
/// `permission_ids` key, and `Form` (serde_urlencoded) refuses a repeated key
/// as a duplicate field, so this endpoint reads the raw body instead — see
/// `parse_matrix_form`. The body is buffered through the handler's OWN limit,
/// not the extractor's 2 MB default: an oversized form answers the app's
/// Spanish JSON refusal (413), never the extractor's English plain-text
/// error. The form is tiny — the limit exists to refuse abuse.
const MATRIX_FORM_BODY_LIMIT: usize = 64 * 1024;
const MATRIX_FORM_TOO_LARGE_MESSAGE: &str = "El cuerpo de la petición es demasiado grande.";

#[derive(Debug, Default)]
pub struct MatrixForm {
    pub role_id: i64,
    pub permission_ids: Vec<i64>,
}

/// Parse the matrix form out of the raw urlencoded body, one pass, no new
/// dependency — the same parser the users screen's assignment form carries,
/// with `permission_ids` in place of `role_ids`: every missing field stays at
/// its default, a malformed id is a form error naming the field, other keys
/// are ignored, and a REPEATED `role_id` is refused — the checkboxes repeat
/// only `permission_ids`, so a second `role_id` can only come from a
/// hand-built request, and silently keeping the last value would let it pick
/// a different role than the one the checkboxes were rendered for.
///
/// `permission_ids=` with an EMPTY value is the empty set, not a form error:
/// the UI omits the key when nothing is ticked, so a well-formed body
/// carrying the key with no value means exactly what the omission means —
/// none. (The protected role's matrix is refused by the service before this
/// matters; for an editable role the empty set is the well-formed spelling of
/// "no permissions".) A genuinely malformed value (`%`, `%zz`, non-numeric)
/// is still refused.
fn parse_matrix_form(body: &[u8]) -> AppResult<MatrixForm> {
    let mut form = MatrixForm::default();
    let mut role_id_seen = false;
    for pair in body.split(|b| *b == b'&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, |b| *b == b'=');
        let key = percent_decode(parts.next().unwrap_or(&[]));
        match key.as_str() {
            "role_id" => {
                let value = percent_decode(parts.next().unwrap_or(&[]));
                if role_id_seen {
                    return Err(AppError::Validation(
                        "El identificador del rol es inválido.".into(),
                    ));
                }
                role_id_seen = true;
                form.role_id = value.trim().parse().map_err(|_| {
                    AppError::Validation("El identificador del rol es inválido.".into())
                })?;
            }
            "permission_ids" => {
                let value = percent_decode(parts.next().unwrap_or(&[]));
                // A present-but-empty value is the empty set: no ids, none.
                if value.trim().is_empty() {
                    continue;
                }
                let id: i64 = value.trim().parse().map_err(|_| {
                    AppError::Validation("Uno de los permisos indicados es inválido.".into())
                })?;
                form.permission_ids.push(id);
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

async fn web_set_matrix(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityRolesManage>,
    body: Body,
) -> AppResult<Response> {
    let bytes = axum::body::to_bytes(body, MATRIX_FORM_BODY_LIMIT)
        .await
        .map_err(|e| {
            if e.to_string().contains("length limit") {
                AppError::PayloadTooLarge(MATRIX_FORM_TOO_LARGE_MESSAGE.into())
            } else {
                AppError::Internal(e.to_string())
            }
        })?;
    let form = parse_matrix_form(&bytes)?;
    state
        .identity_service
        .set_role_matrix(
            &permissions_repo(&state),
            principal.user_id,
            form.role_id,
            &form.permission_ids,
        )
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "role-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/roles").into_response())
}

// ---------------------------------------------------------------------------
// Dialog fragment (id-final, like the users screen's dialog forms)
// ---------------------------------------------------------------------------

/// The edit dialog's fragment: the details form and the matrix for ONE role.
/// The fragment endpoint carries the same gate the submit endpoints answer,
/// so a principal that cannot edit never sees the form it could not use.
/// The protected role renders read-only — the interface never offers the
/// actions the triggers refuse.
async fn web_role_edit_form(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    _: Require<IdentityRolesManage>,
) -> AppResult<Html<String>> {
    let matrix = state.identity_service.role_matrix(&permissions_repo(&state), id).await?;
    let groups = module_groups(matrix.catalog, &matrix.held_ids);
    Ok(Html(
        render(RoleEditFormPartial {
            description_value: matrix.role.description.clone().unwrap_or_default(),
            role: matrix.role,
            groups,
        })?,
    ))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/roles", get(roles_page))
        .route(
            "/web/roles",
            get(web_role_list).post(web_create_role),
        )
        .route("/web/roles/edit", post(web_edit_role))
        .route("/web/roles/delete", post(web_delete_role))
        .route("/web/roles/matrix", post(web_set_matrix))
        .route("/web/roles/edit-form/{id}", get(web_role_edit_form))
}

// ---------------------------------------------------------------------------
// Tests (AC13 / AC15 / AC17 / the self-lockout / the gating and round trips)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use crate::models::NewUserRole;
    use crate::repositories::role_repo::{RoleRepository, SqliteRoleRepository};
    use crate::repositories::{PermissionRepository, SqlitePermissionRepository};
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
        // Fixture wiring (S5): the permissionless variant. The shared principal
        // this screen's tests build on holds NO roles — the refusal fixtures
        // and the deliberately-narrow principals depend on that premise, and
        // every grant here comes through app_with_permissions' custom role.
        // The full-permission seed (seed_session) is for the department
        // fixtures, not this one.
        test_support::seed_session_without_roles(&pool).await.unwrap();
        pool
    }

    /// The full production router over the shared fixture (TEST_COOKIE's
    /// user). `with_permissions` grants that user a custom role carrying the
    /// given permission codes through the real grant path, so the bootstrap
    /// administrator stays the only protected-role holder and the AC14
    /// arithmetic stays observable.
    async fn app_with_permissions(permissions: &[&str]) -> (axum::Router, crate::routes::AppState) {
        let pool = test_pool().await;
        let state = test_support::app_state(pool.clone());
        if !permissions.is_empty() {
            sqlx::query("INSERT INTO roles (code, name) VALUES ('operador', 'Operador')")
                .execute(&pool)
                .await
                .unwrap();
            for code in permissions {
                sqlx::query(
                    "INSERT INTO role_permissions (role_id, permission_id) \
                     SELECT r.id, p.id FROM roles r JOIN permissions p ON p.code = ? \
                     WHERE r.code = 'operador'",
                )
                .bind(code)
                .execute(&pool)
                .await
                .unwrap();
            }
            let roles = SqliteRoleRepository::new(pool.clone());
            let role = roles.find_by_code("operador").await.unwrap().unwrap();
            let user_id: i64 =
                sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
                    .bind(test_support::TEST_USERNAME)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            roles
                .grant(&NewUserRole {
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
            .oneshot(
                builder
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
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

    async fn role_id_by_code(pool: &sqlx::SqlitePool, code: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM roles WHERE code = ?")
            .bind(code)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn permission_id_by_code(pool: &sqlx::SqlitePool, code: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM permissions WHERE code = ?")
            .bind(code)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The matrix one role currently holds, as sorted codes — the
    /// "nothing was written" proof.
    async fn held_codes(pool: &sqlx::SqlitePool, role_id: i64) -> Vec<String> {
        let mut codes = SqlitePermissionRepository::new(pool.clone())
            .codes_for_role(role_id)
            .await
            .unwrap();
        codes.sort();
        codes
    }

    async fn create_role_via_screen(app: &axum::Router, code: &str) -> axum::http::Response<Body> {
        send(
            app,
            "POST",
            "/web/roles",
            &form_headers(),
            &format!("code={code}&name=El+rol+{code}&description= creado por la pantalla"),
        )
        .await
    }

    // -- the gate ---------------------------------------------------------------

    #[tokio::test]
    async fn a_full_page_visit_without_the_permission_gets_the_html_refusal() {
        // The shared fixture user holds no roles at all.
        let (app, _state) = app_with_permissions(&[]).await;
        let resp = send(&app, "GET", "/roles", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        let html = body_string(resp).await;
        assert!(html.contains("Acción no permitida"), "{html:.400}");
        assert!(
            html.contains("identity.roles.manage"),
            "the refusal names the missing permission: {html:.400}"
        );
    }

    /// The catalog has no `identity.roles.read`, so every surface on this
    /// screen — reads included — refuses a principal without
    /// `identity.roles.manage` (the deliberate mapping this slice writes into
    /// the spec).
    #[tokio::test]
    async fn a_principal_without_the_permission_is_refused_on_the_page_and_on_every_mutation() {
        let (app, state) = app_with_permissions(&[]).await;
        let vendedor_id = role_id_by_code(&state.pool, "vendedor").await;
        let before = held_codes(&state.pool, vendedor_id).await;
        let roles_before: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM roles").fetch_one(&state.pool).await.unwrap();

        // The page and its fragment.
        for uri in ["/roles", "/web/roles"] {
            let resp = send(&app, "GET", uri, &cookie(), "").await;
            assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN, "GET {uri}");
        }
        // Every mutation, in the HTMX JSON shape the notice box renders.
        let mutations: [(&str, String); 4] = [
            (
                "/web/roles",
                "code=fantasma&name=Fantasma".to_string(),
            ),
            (
                "/web/roles/edit",
                format!("role_id={vendedor_id}&name=Otro"),
            ),
            ("/web/roles/delete", format!("role_id={vendedor_id}")),
            (
                "/web/roles/matrix",
                format!("role_id={vendedor_id}&permission_ids="),
            ),
        ];
        for (uri, body) in mutations {
            let resp = send(&app, "POST", uri, &form_headers(), &body).await;
            assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN, "POST {uri}");
            let json: serde_json::Value =
                serde_json::from_str(&body_string(resp).await).unwrap();
            assert!(
                json.get("error")
                    .and_then(|e| e.as_str())
                    .is_some_and(|m| m.contains("identity.roles.manage")),
                "POST {uri}: {json}"
            );
        }
        // The fragment endpoint refuses too.
        let resp = send(&app, "GET", "/web/roles/edit-form/1", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);

        // Nothing was written anywhere.
        assert_eq!(before, held_codes(&state.pool, vendedor_id).await);
        let roles_after: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM roles").fetch_one(&state.pool).await.unwrap();
        assert_eq!(roles_after, roles_before);
    }

    /// The gate case the model actually cares about: a principal holding the
    /// USERS tier (`identity.users.read` + `identity.users.manage`) but NOT
    /// `identity.roles.manage` is refused every surface here — the empty
    /// principal above cannot separate "no permission at all" from "the wrong
    /// permission". The same fixture user reaches `/users` (it holds the
    /// users read), proving the refusal is this screen's gate and not a
    /// broken fixture.
    #[tokio::test]
    async fn a_users_manage_only_principal_is_refused_the_roles_screen_and_its_matrix() {
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
        ])
        .await;
        let vendedor_id = role_id_by_code(&state.pool, "vendedor").await;
        let before = held_codes(&state.pool, vendedor_id).await;

        // The fixture really holds the users tier: /users is reachable.
        let resp = send(&app, "GET", "/users", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // The roles page: the full-page HTML refusal, naming the gate.
        let resp = send(&app, "GET", "/roles", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
        let html = body_string(resp).await;
        assert!(html.contains("Acción no permitida"), "{html:.400}");
        assert!(
            html.contains("identity.roles.manage"),
            "the refusal names the missing permission: {html:.400}"
        );

        // The matrix mutation: the HTMX JSON shape, same missing permission.
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={vendedor_id}&permission_ids="),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|m| m.contains("identity.roles.manage")),
            "{json}"
        );

        // Nothing was written anywhere.
        assert_eq!(before, held_codes(&state.pool, vendedor_id).await);
    }

    // -- the screen round trips -------------------------------------------------

    #[tokio::test]
    async fn the_page_lists_the_roles_their_descriptions_and_the_users_that_hold_them() {
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.roles.manage",
        ])
        .await;
        state.identity_service.bootstrap_admin(Some(ADMIN_PASSWORD)).await.unwrap();
        let permissions = SqlitePermissionRepository::new(state.pool.clone());
        let vendedor = state.identity_service.role_list().await.unwrap().into_iter()
            .find(|r| r.code == "vendedor").unwrap();
        let actor_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(test_support::TEST_USERNAME)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        // A user holds vendedor: the list shows the count (and the deletion
        // refusal names them).
        let created = state
            .identity_service
            .create_user("caja1", "Caja Uno", "initial password 1")
            .await
            .unwrap();
        state
            .identity_service
            .assign_roles(&permissions, actor_id, created.id, &[vendedor.id])
            .await
            .unwrap();

        let resp = send(&app, "GET", "/roles", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let html = body_string(resp).await;
        for expected in [
            "admin",
            "Administrador",
            "Ventas y clientes; consulta de stock.",
            "1 usuario",
            "protegido",
            "Nuevo rol",
            "data-nav=\"roles\"",
        ] {
            assert!(html.contains(expected), "page must show {expected}: {html:.600}");
        }
        // The fragment: same list, smaller body.
        let resp = send(&app, "GET", "/web/roles", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("Cajero"), "{html:.400}");
        assert!(html.contains("Depósito"), "{html:.400}");
    }

    #[tokio::test]
    async fn create_role_and_tick_a_permission_round_trip_through_the_screen() {
        let (app, state) =
            app_with_permissions(&["identity.roles.manage"]).await;

        // Create through the collection endpoint.
        let resp = create_role_via_screen(&app, "supervisor").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let trigger = resp
            .headers()
            .get("HX-Trigger")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let html = body_string(resp).await;
        assert!(html.contains("supervisor"), "{html:.400}");
        assert_eq!(trigger, "role-created", "the create answers with its event");
        let role_id = role_id_by_code(&state.pool, "supervisor").await;
        let created = SqliteRoleRepository::new(state.pool.clone())
            .find_by_id(role_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!created.is_system, "a created role is ordinary");
        assert_eq!(created.description.as_deref(), Some("creado por la pantalla"));

        // A duplicate code is refused with the Spanish conflict.
        let resp = create_role_via_screen(&app, "supervisor").await;
        assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(
            json.get("error").and_then(|e| e.as_str()).is_some_and(|m| m.contains("Ya existe un rol")),
            "{json}"
        );

        // A malformed code shape is refused with the rule, nothing written.
        for code in ["Vendedor", "a", "con espacio", "número-malo"] {
            let resp = send(
                &app,
                "POST",
                "/web/roles",
                &form_headers(),
                &format!("code={code}&name=Nombre"),
            )
            .await;
            assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST, "code={code}");
            let json: serde_json::Value =
                serde_json::from_str(&body_string(resp).await).unwrap();
            assert!(
                json.get("error").and_then(|e| e.as_str()).is_some_and(|m| m.contains("código del rol")),
                "code={code}: {json}"
            );
        }
        let roles_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM roles").fetch_one(&state.pool).await.unwrap();
        // The four seeded roles + the fixture's operador + the created
        // supervisor: the refused creations added nothing.
        assert_eq!(roles_count, 6, "the refused creations wrote nothing");

        // Tick one permission through the matrix endpoint.
        let dashboard = permission_id_by_code(&state.pool, "dashboard.read").await;
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={role_id}&permission_ids={dashboard}"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            held_codes(&state.pool, role_id).await,
            vec!["dashboard.read".to_string()]
        );

        // The edit form fragment renders the matrix with the tick and the
        // descriptions the operator reads before granting.
        let resp = send(
            &app,
            "GET",
            &format!("/web/roles/edit-form/{role_id}"),
            &cookie(),
            "",
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("Ver el panel principal"), "{html:.600}");
        assert!(html.contains("name=\"permission_ids\""), "{html:.600}");
        assert!(
            html.contains(&format!("value=\"{dashboard}\" checked")),
            "the held permission is pre-ticked: {html:.600}"
        );

        // Editing the details through the collection endpoint.
        let resp = send(
            &app,
            "POST",
            "/web/roles/edit",
            &form_headers(),
            &format!("role_id={role_id}&name=Supervisor&description=Supervisa"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let edited = SqliteRoleRepository::new(state.pool.clone())
            .find_by_id(role_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(edited.name, "Supervisor");
        assert_eq!(edited.description.as_deref(), Some("Supervisa"));
        // The matrix survives the details edit.
        assert_eq!(
            held_codes(&state.pool, role_id).await,
            vec!["dashboard.read".to_string()]
        );

        // Deleting the role nobody holds succeeds.
        let resp = send(
            &app,
            "POST",
            "/web/roles/delete",
            &form_headers(),
            &format!("role_id={role_id}"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert!(
            SqliteRoleRepository::new(state.pool.clone())
                .find_by_id(role_id)
                .await
                .unwrap()
                .is_none(),
            "the unheld role is gone"
        );
    }

    // -- AC17: a matrix edit applies to the very next request -------------------

    /// The matrix edit goes through the screen's real endpoint and the change
    /// is visible on the NEXT request (no restart): ticking
    /// `identity.users.read` ONTO the actor's own custom role — keeping its
    /// `identity.roles.manage`, because stripping it is exactly the
    /// self-lockout the service refuses — moves a `GET /users` from the 403
    /// refusal to 200, and removing it returns the refusal.
    #[tokio::test]
    async fn ac17_a_matrix_edit_through_the_screen_applies_to_the_next_request() {
        let (app, state) = app_with_permissions(&["identity.roles.manage"]).await;
        let operador_id = role_id_by_code(&state.pool, "operador").await;
        let users_read = permission_id_by_code(&state.pool, "identity.users.read").await;
        let roles_manage = permission_id_by_code(&state.pool, "identity.roles.manage").await;

        // Without the permission: the users page refuses.
        let resp = send(&app, "GET", "/users", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);

        // Tick identity.users.read onto the role through the matrix endpoint,
        // keeping the tier the actor holds.
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={operador_id}&permission_ids={roles_manage}&permission_ids={users_read}"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // The very next request honours it: no restart, no cache.
        let resp = send(&app, "GET", "/users", &cookie(), "").await;
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "the matrix edit must apply to the next request"
        );

        // Removing the added permission (keeping the tier) is refused nowhere:
        // the next request is refused again.
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={operador_id}&permission_ids={roles_manage}"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let resp = send(&app, "GET", "/users", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);

        // And stripping the tier the actor itself holds is the self-lockout
        // refusal — the same endpoint, the rule this slice adds.
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={operador_id}&permission_ids={users_read}"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(
            json.get("error").and_then(|e| e.as_str()).is_some_and(|m| m.contains("No podés")),
            "{json}"
        );
    }

    // -- AC13: the protected role is locked through the screen ------------------

    #[tokio::test]
    async fn ac13_the_protected_role_is_locked_through_the_screen() {
        let (app, state) =
            app_with_permissions(&["identity.roles.manage"]).await;
        let admin_id = role_id_by_code(&state.pool, "admin").await;

        // The list marks it as protected: no delete button is offered for it.
        let resp = send(&app, "GET", "/web/roles", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let html = body_string(resp).await;
        assert!(
            html.contains("protegido"),
            "the protected flag reaches the operator: {html:.400}"
        );
        // The fragment button for the protected row is the read-only "Ver".
        assert!(html.contains("Ver"), "{html:.400}");

        // The matrix edit is refused with the Spanish reason, nothing written.
        let matrix_before = held_codes(&state.pool, admin_id).await;
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={admin_id}&permission_ids="),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        let message = json.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(message.contains("rol protegido"), "{json}");
        assert!(
            message.contains("matriz"),
            "the refusal explains the matrix rule: {json}"
        );
        assert_eq!(
            matrix_before,
            held_codes(&state.pool, admin_id).await,
            "the refused matrix edit writes nothing"
        );

        // The delete is refused with the Spanish reason, nothing written.
        let resp = send(
            &app,
            "POST",
            "/web/roles/delete",
            &form_headers(),
            &format!("role_id={admin_id}"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(
            json.get("error").and_then(|e| e.as_str()).is_some_and(|m| m.contains("rol protegido")),
            "{json}"
        );
        assert!(
            SqliteRoleRepository::new(state.pool.clone())
                .find_by_id(admin_id)
                .await
                .unwrap()
                .is_some(),
            "the refused deletion writes nothing"
        );

        // The code rename is not offered: the edit form carries no code field
        // for any role, so the interface cannot offer what the trigger
        // refuses. (The refusal's mapping itself is proven in
        // `role_repo::map_db_err`'s tests and the service layer keeps the
        // statement away from the trigger by construction.)
        let resp = send(
            &app,
            "GET",
            &format!("/web/roles/edit-form/{admin_id}"),
            &cookie(),
            "",
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let html = body_string(resp).await;
        assert!(
            !html.contains("name=\"permission_ids\""),
            "the protected role's matrix must not be editable: {html:.600}"
        );
        assert!(
            !html.contains("Eliminar"),
            "the protected role must not be offered a deletion: {html:.600}"
        );
        assert!(
            html.contains("no se puede eliminar"),
            "the dialog explains the lock: {html:.600}"
        );
    }

    /// The full catalog still reaches the protected role's read-only matrix:
    /// the 23 codes grouped by module, every one with its Spanish description
    /// (the seeded descriptions corrected in the previous slice are what the
    /// operator finally reads here).
    #[tokio::test]
    async fn the_matrix_renders_the_whole_catalog_with_descriptions() {
        let (app, state) =
            app_with_permissions(&["identity.roles.manage"]).await;
        let admin_id = role_id_by_code(&state.pool, "admin").await;
        let resp = send(
            &app,
            "GET",
            &format!("/web/roles/edit-form/{admin_id}"),
            &cookie(),
            "",
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let html = body_string(resp).await;
        for expected in [
            "dashboard.read",
            "finance.methods.manage",
            "inventory.stock.write",
            "sales.cancel",
            "customers.collect",
            "purchases.costs.read",
            "suppliers.write",
            "identity.users.read",
            "identity.roles.manage",
            "Ver el panel principal",
            "Administrar cuentas y medios de pago",
            "Ajustar stock",
            "Anular ventas",
            "Registrar cobros",
            "Ver costos por proveedor",
            "Crear y editar proveedores",
            "Crear usuarios y restablecer contraseñas de cuentas sin roles protegidos",
            "Crear roles, editar la matriz de permisos y cambiar los roles de otras cuentas",
        ] {
            assert!(html.contains(expected), "matrix must show {expected}: {html:.800}");
        }
    }

    // -- AC15: a held role cannot be deleted, and the refusal names the users --

    #[tokio::test]
    async fn ac15_deleting_a_held_role_is_refused_and_names_the_blocking_users() {
        let (app, state) =
            app_with_permissions(&["identity.roles.manage"]).await;
        let permissions = SqlitePermissionRepository::new(state.pool.clone());
        let vendedor = state.identity_service.role_list().await.unwrap().into_iter()
            .find(|r| r.code == "vendedor").unwrap();
        let created = state
            .identity_service
            .create_user("caja1", "Caja Uno", "initial password 1")
            .await
            .unwrap();
        let actor_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(test_support::TEST_USERNAME)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        state
            .identity_service
            .assign_roles(&permissions, actor_id, created.id, &[vendedor.id])
            .await
            .unwrap();

        let resp = send(
            &app,
            "POST",
            "/web/roles/delete",
            &form_headers(),
            &format!("role_id={}", vendedor.id),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        let message = json.get("error").and_then(|e| e.as_str()).unwrap_or("");
        // The refusal NAMES the users that block it.
        assert!(message.contains("caja1"), "{json}");
        assert!(message.contains("No se puede eliminar"), "{json}");
        assert!(message.contains("quitáselo"), "{json}");
        assert!(!message.contains("FOREIGN KEY"), "no raw SQL text: {json}");
        assert!(
            SqliteRoleRepository::new(state.pool.clone())
                .find_by_id(vendedor.id)
                .await
                .unwrap()
                .is_some(),
            "the refused deletion writes nothing"
        );
    }

    // -- the matrix self-lockout rule (the new hole this slice closes) --------

    /// An actor holding `identity.roles.manage` THROUGH a role is refused a
    /// matrix edit that removes it from that role: without the rule the
    /// holder strips its own tier and locks itself (and everyone sharing the
    /// role) out of the administration on the next request. Nothing is
    /// written, and the same edit on a role the actor does NOT hold succeeds.
    #[tokio::test]
    async fn the_matrix_self_lockout_is_refused_and_writes_nothing() {
        let (app, state) =
            app_with_permissions(&["identity.roles.manage"]).await;
        let operador_id = role_id_by_code(&state.pool, "operador").await;
        let roles_manage = permission_id_by_code(&state.pool, "identity.roles.manage").await;
        let dashboard = permission_id_by_code(&state.pool, "dashboard.read").await;

        // The self-lockout: the actor holds the role being stripped.
        let before = held_codes(&state.pool, operador_id).await;
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={operador_id}&permission_ids={dashboard}"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        let message = json.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(message.contains("No podés"), "{json}");
        assert!(message.contains("identity.roles.manage"), "{json}");
        assert!(message.contains("rol"), "{json}");
        assert_eq!(
            before,
            held_codes(&state.pool, operador_id).await,
            "the refused edit writes nothing"
        );
        // The actor's own access is untouched: the roles screen still answers.
        let resp = send(&app, "GET", "/roles", &cookie(), "").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // The same edit on a role the actor does NOT hold succeeds (an
        // ordinary role losing its tier is legitimate administration).
        let created_role = create_role_via_screen(&app, "visor").await;
        assert_eq!(created_role.status(), axum::http::StatusCode::OK);
        let visor_id = role_id_by_code(&state.pool, "visor").await;
        let actor_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(test_support::TEST_USERNAME)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let created = state
            .identity_service
            .create_user("visor-holder", "V", "initial password 1")
            .await
            .unwrap();
        SqliteRoleRepository::new(state.pool.clone())
            .grant(&NewUserRole {
                user_id: created.id,
                role_id: visor_id,
                granted_by: actor_id,
            })
            .await
            .unwrap();
        // Give visor the tier first, then remove it: the actor does not hold
        // the role, so the edit is legitimate and must succeed.
        let permissions = SqlitePermissionRepository::new(state.pool.clone());
        permissions
            .set_role_permissions(visor_id, &[roles_manage, dashboard])
            .await
            .unwrap();
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={visor_id}&permission_ids={dashboard}"),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            held_codes(&state.pool, visor_id).await,
            vec!["dashboard.read".to_string()],
            "the legitimate removal succeeded"
        );
    }

    // -- the form parser and its limit ---------------------------------------

    /// An oversized matrix-form body is the app's Spanish refusal (413), not
    /// the extractor's English plain-text buffering error.
    #[tokio::test]
    async fn an_oversized_matrix_form_body_answers_413_in_the_app_shape() {
        let (app, _state) =
            app_with_permissions(&["identity.roles.manage"]).await;
        let oversized = format!("role_id=1&junk={}", "x".repeat(128 * 1024));
        let resp = send(&app, "POST", "/web/roles/matrix", &form_headers(), &oversized).await;
        assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("error").and_then(|e| e.as_str()).is_some_and(|m| m.contains("demasiado grande")),
            "{json}"
        );
        assert!(
            !body.contains("Failed to buffer"),
            "the English plain-text shape must be gone: {body:.200}"
        );
    }

    /// A hand-built request repeating `role_id` is refused: silently keeping
    /// the last value would let it pick a different role than the one the
    /// checkboxes were rendered for.
    #[tokio::test]
    async fn a_duplicated_role_id_in_the_matrix_form_is_refused() {
        let (app, _state) =
            app_with_permissions(&["identity.roles.manage"]).await;
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            "role_id=1&role_id=2",
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(
            json.get("error").and_then(|e| e.as_str()).is_some_and(|m| m.contains("identificador del rol")),
            "{json}"
        );
    }

    /// A present-but-empty `permission_ids=` value is the empty set, not a
    /// 400 (the UI omits the key when nothing is ticked; both spellings mean
    /// none), while genuinely malformed values stay refused.
    #[tokio::test]
    async fn a_present_but_empty_permission_ids_value_is_an_empty_set_not_a_400() {
        let (app, state) =
            app_with_permissions(&["identity.roles.manage"]).await;
        let visor = create_role_via_screen(&app, "visor").await;
        assert_eq!(visor.status(), axum::http::StatusCode::OK);
        let visor_id = role_id_by_code(&state.pool, "visor").await;
        let dashboard = permission_id_by_code(&state.pool, "dashboard.read").await;
        let permissions = SqlitePermissionRepository::new(state.pool.clone());
        permissions
            .set_role_permissions(visor_id, &[dashboard])
            .await
            .unwrap();

        // The empty value replaces the held matrix with none.
        let resp = send(
            &app,
            "POST",
            "/web/roles/matrix",
            &form_headers(),
            &format!("role_id={visor_id}&permission_ids="),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert!(held_codes(&state.pool, visor_id).await.is_empty());

        // A genuinely malformed value is still a form error.
        for value in ["%", "%zz", "not-a-number"] {
            let resp = send(
                &app,
                "POST",
                "/web/roles/matrix",
                &form_headers(),
                &format!("role_id={visor_id}&permission_ids={value}"),
            )
            .await;
            assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST, "permission_ids={value}");
            let json: serde_json::Value =
                serde_json::from_str(&body_string(resp).await).unwrap();
            assert!(
                json.get("error").and_then(|e| e.as_str()).is_some_and(|m| m.contains("permisos indicados")),
                "permission_ids={value}: {json}"
            );
        }
    }

    // -- the catalog census holds across the matrix reads ----------------------

    /// The matrix read returns exactly the 23 seeded codes (AC12's set-level
    /// shape), and the round trip of ticking one permission keeps the rest
    /// out — the editor replaces the WHOLE matrix, never appends.
    #[tokio::test]
    async fn the_matrix_editor_replaces_the_whole_set() {
        let (app, state) =
            app_with_permissions(&["identity.roles.manage"]).await;
        let visor = create_role_via_screen(&app, "visor").await;
        assert_eq!(visor.status(), axum::http::StatusCode::OK);
        let visor_id = role_id_by_code(&state.pool, "visor").await;
        let sales = ["sales.read", "sales.create"];
        let mut ids = Vec::new();
        for code in sales {
            ids.push(permission_id_by_code(&state.pool, code).await);
        }
        let body = format!("role_id={visor_id}&{}", ids.iter().map(|id| format!("permission_ids={id}")).collect::<Vec<_>>().join("&"));
        let resp = send(&app, "POST", "/web/roles/matrix", &form_headers(), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let held = held_codes(&state.pool, visor_id).await;
        assert_eq!(held.len(), 2, "exactly the two submitted permissions");
        for code in sales {
            assert!(held.contains(&code.to_string()), "{code} must be held");
        }
        // The full matrix read carries the whole 23-row catalog.
        let matrix = state
            .identity_service
            .role_matrix(&SqlitePermissionRepository::new(state.pool.clone()), visor_id)
            .await
            .unwrap();
        assert_eq!(matrix.catalog.len(), 23, "the catalog is the whole 23");
        assert_eq!(matrix.held_ids.len(), 2, "the held ids are the submitted set");
    }
}
