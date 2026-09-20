// Test-only identity support (part 1 of the S1b slice split). Every HTTP test
// module authenticates with ONE fixed session token: `seed_session` inserts a
// real user plus a live session through the real repositories, and
// `with_cookie`/`TEST_COOKIE` put the token on the request. Part 2 turned the
// deny-by-default gate on, so these requests authenticate the way a client
// does: through the cookie, resolved by the same `IdentityService` production
// uses. No test-only bypass exists (AC24); `app_state` below is the separate
// light-hasher construction path part 2 asked for.
use axum::http::request::Builder;
use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::{NewRole, NewSession, NewUser, NewUserRole};
use crate::repositories::{
    PermissionRepository, RoleRepository, SessionRepository, SqlitePermissionRepository,
    SqliteRoleRepository, SqliteSessionRepository, SqliteUserRepository, UserRepository,
};
use crate::security::password::PasswordHasher;
use crate::security::session::{hash_token, mint_token, SessionPolicy, SESSION_COOKIE};
use crate::services::identity::{IdentityService, SystemClock, ThrottleConfig};

/// The one session token every HTTP test authenticates with. Fixed so the
/// cookie value can be a compile-time constant; only its sha256 digest is
/// stored, exactly like the production path.
pub const TEST_SESSION_TOKEN: &str = "test-session-token-roya-identity";

/// The `Cookie` header value carrying [`TEST_SESSION_TOKEN`]. Duplicated from
/// `SESSION_COOKIE` (a `const` cannot be `concat!`-built from another `const`),
/// with the drift guarded two ways below: a compile-time assertion and the
/// plumbing test's round-trip through the real cookie parser.
pub const TEST_COOKIE: &str = "roya_session=test-session-token-roya-identity";

/// The migration's sentinel account username (migration 30): the system actor
/// every pre-existing row and migration-time seed is attributed to.
pub const AUDIT_SYSTEM_USERNAME: &str = "sistema";

/// The id of the migration-created sentinel account. Test call sites that only
/// need A valid acting user resolve it through the real repository read; the
/// audit attribution tests create dedicated users instead, because the point
/// there is telling two actors apart.
pub async fn audit_actor_id(pool: &SqlitePool) -> AppResult<i64> {
    sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(AUDIT_SYSTEM_USERNAME)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            crate::error::AppError::Internal(
                "the migration's system sentinel user is missing: migrations must run first"
                    .to_string(),
            )
        })
}

/// Insert one dedicated audit-actor user through the real repository write
/// path (active, placeholder hash — these tests exercise attribution, not
/// credentials). Returns the user id, so a test can assert that the audited
/// row carries exactly this actor and not another one.
pub async fn seed_audit_user(
    pool: &SqlitePool,
    username: &str,
    display_name: &str,
) -> AppResult<i64> {
    let users = SqliteUserRepository::new(pool.clone());
    let user = users
        .create(&NewUser {
            username: username.to_string(),
            display_name: display_name.to_string(),
            password_hash: "placeholder-not-a-real-argon2-hash".to_string(),
            must_change_password: false,
        })
        .await?;
    Ok(user.id)
}

/// The fixed username `seed_session` creates (matches the users-table CHECK
/// shape: 3-64 lowercase chars with dots/underscores/hyphens in the middle).
pub const TEST_USERNAME: &str = "test-admin";

/// Far-future TTL for the seeded session: the suite must never race an expiry.
const TEST_TTL_HOURS: i64 = 24 * 365 * 10;

/// Compile-time guard: TEST_COOKIE pins `SESSION_COOKIE`, so a cookie-name
/// change cannot leave a silently stale test constant behind.
const fn test_cookie_name_matches_production() -> bool {
    let expected = SESSION_COOKIE.as_bytes();
    let pinned = b"roya_session";
    if expected.len() != pinned.len() {
        return false;
    }
    let mut i = 0;
    while i < expected.len() {
        if expected[i] != pinned[i] {
            return false;
        }
        i += 1;
    }
    true
}
const _: () = assert!(test_cookie_name_matches_production());

/// Insert the fixed test user (active, placeholder password hash — nothing
/// verifies it in this part) and one live session whose `token_hash` is the
/// digest of [`TEST_SESSION_TOKEN`] under the same hashing the production path
/// uses (`security::session::hash_token`). Goes through the real
/// `SqliteUserRepository` / `SqliteSessionRepository` write path, never raw
/// SQL. Returns the inserted session id.
///
/// Since S5 the user holds every permission in the catalog through a real
/// role grant, so the department routes' `Require<P>` is satisfied for the
/// shared fixture. The role is a custom one (`probe_all`) holding all 23
/// codes, granted through the real `SqliteRoleRepository` write path — the
/// same grant shape the bootstrap performs (`roles.grant` with
/// `granted_by` = the user itself, idempotent). Deliberately NOT the
/// protected `admin` role: the identity screens' fixtures bootstrap the one
/// protected administrator themselves (AC14's arithmetic counts protected
/// holders), so a second protected holder seeded here would collide with
/// them. A test that must hold a GIVEN set — or none — builds its own: see
/// [`seed_session_without_roles`] and [`seed_session_with_permissions`].
pub async fn seed_session(pool: &SqlitePool) -> AppResult<i64> {
    let session_id = seed_session_without_roles(pool).await?;
    let user_id = user_id(pool).await?;
    let codes: Vec<&str> = crate::security::authz::PERMISSIONS.to_vec();
    grant_role_with_codes(pool, "probe_all", "Probe All", user_id, &codes).await?;
    Ok(session_id)
}

/// The permissionless variant of [`seed_session`]: the same user and live
/// session with NO roles, so the resolved principal holds no permission at
/// all. The kernel's union/no-roles tests and the identity screens' refusal
/// fixtures build on it — `seed_session` grants the protected role, so a
/// fixture that needs a principal LACKING permissions must use this one.
pub async fn seed_session_without_roles(pool: &SqlitePool) -> AppResult<i64> {
    let policy = SessionPolicy::new(TEST_TTL_HOURS, false);
    let now = crate::services::identity::Clock::now(&crate::services::identity::SystemClock);
    let users = SqliteUserRepository::new(pool.clone());
    let sessions = SqliteSessionRepository::new(pool.clone());
    let user = users
        .create(&NewUser {
            username: TEST_USERNAME.to_string(),
            display_name: "Test Admin".to_string(),
            password_hash: "placeholder-not-a-real-argon2-hash".to_string(),
            must_change_password: false,
        })
        .await?;
    let session = sessions
        .insert(&NewSession {
            token_hash: hash_token(TEST_SESSION_TOKEN),
            user_id: user.id,
            expires_at: policy.expires_at(now),
            last_seen_at: now,
            user_agent: None,
        })
        .await?;
    Ok(session.id)
}

/// One extra session whose user holds exactly `permissions` (a custom role
/// granted through the real grant path), returning the raw token — the cookie
/// value a test puts on its own requests, so a suite can drive a
/// read-only/forbidden principal NEXT TO the shared full-permission one.
pub async fn seed_session_with_permissions(
    pool: &SqlitePool,
    permissions: &[&str],
) -> AppResult<String> {
    let policy = SessionPolicy::new(TEST_TTL_HOURS, false);
    let now = crate::services::identity::Clock::now(&crate::services::identity::SystemClock);
    let users = SqliteUserRepository::new(pool.clone());
    let sessions = SqliteSessionRepository::new(pool.clone());
    let user = users
        .create(&NewUser {
            // A per-call suffix: a test can seed more than one probe principal
            // in the same pool (e.g. one read-only, one holding the permission
            // under test), and usernames are unique case-insensitively.
            username: probe_username().to_string(),
            display_name: "Test Probe".to_string(),
            password_hash: "placeholder-not-a-real-argon2-hash".to_string(),
            must_change_password: false,
        })
        .await?;
    let codes: Vec<&str> = permissions.to_vec();
    grant_role_with_codes(pool, &probe_role_code(), "Probe Set", user.id, &codes).await?;
    let token = mint_token()?;
    sessions
        .insert(&NewSession {
            token_hash: hash_token(&token),
            user_id: user.id,
            expires_at: policy.expires_at(now),
            last_seen_at: now,
            user_agent: None,
        })
        .await?;
    Ok(token)
}

/// Create a custom role, load its permission matrix from the catalog codes,
/// and grant it to one user — the real repository write path, never raw SQL,
/// with the ids resolved in one statement (`set_role_permissions`). Returns
/// the role id. The role code must be free in the test database (fresh pool
/// per test). A requested code missing from the seeded catalog is an internal
/// test error, never a silent empty matrix.
async fn grant_role_with_codes(
    pool: &SqlitePool,
    role_code: &str,
    role_name: &str,
    user_id: i64,
    codes: &[&str],
) -> AppResult<i64> {
    let roles = SqliteRoleRepository::new(pool.clone());
    let permissions_repo = SqlitePermissionRepository::new(pool.clone());
    let role = roles
        .create(&NewRole {
            code: role_code.to_string(),
            name: role_name.to_string(),
            description: None,
        })
        .await?;
    let mut permission_ids = Vec::with_capacity(codes.len());
    for code in codes.iter() {
        let id: i64 = sqlx::query_scalar("SELECT id FROM permissions WHERE code = ?")
            .bind(code)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| {
                crate::error::AppError::Internal(format!(
                    "test requested unknown permission {code}"
                ))
            })?;
        permission_ids.push(id);
    }
    permissions_repo
        .set_role_permissions(role.id, &permission_ids)
        .await?;
    roles
        .grant(&NewUserRole {
            user_id,
            role_id: role.id,
            granted_by: user_id,
        })
        .await?;
    Ok(role.id)
}

/// The `Cookie` header value for a minted token, so tests driving the
/// [`seed_session_with_permissions`] principal build it in one place.
pub fn cookie_for(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}")
}

/// A fresh username for each probe principal (see
/// [`seed_session_with_permissions`]): the counter is process-local and every
/// test pool is fresh, so plain monotonic suffixes never collide.
fn probe_username() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("test_probe_{n}")
}

/// A fresh role code for each probe principal, for the same reason the
/// usernames are unique: two probes in one pool must not share a row.
fn probe_role_code() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("probe_set_{n}")
}

/// The shared test user's id, read through the real repository (the grant
/// path needs it; keep the read out of the callers).
async fn user_id(pool: &SqlitePool) -> AppResult<i64> {
    let users = SqliteUserRepository::new(pool.clone());
    let user = users
        .find_by_username(TEST_USERNAME)
        .await?
        .ok_or_else(|| crate::error::AppError::Internal(
            "the shared test user is missing".to_string(),
        ))?;
    Ok(user.id)
}

/// Add the test session cookie to a request builder, so the built request is
/// already authenticated for part 2's deny-by-default middleware.
pub fn with_cookie(builder: Builder) -> Builder {
    builder.header(axum::http::header::COOKIE, TEST_COOKIE)
}

/// Test-only `AppState` construction: identical to the production path
/// (`AppState::new` semantics) except the identity service uses
/// [`PasswordHasher::light()`]. This is the separate construction path part 2
/// asked for — production parameters are never silently weakened.
pub fn app_state(pool: SqlitePool) -> crate::routes::AppState {
    let identity = IdentityService::new(
        SqliteUserRepository::new(pool.clone()),
        SqliteSessionRepository::new(pool.clone()),
        SqliteRoleRepository::new(pool.clone()),
        SystemClock,
        PasswordHasher::light(),
        SessionPolicy::new(12, false),
        ThrottleConfig::default(),
    );
    crate::routes::AppState::with_identity_service(pool, false, true, true, identity)
}

/// Seed one extra user flagged `must_change_password` plus one live session
/// for it, returning the raw token (the cookie value). S1b part 2 asserts the
/// flag is NOT enforced yet, so S3's enforcement is a deliberate change.
pub async fn seed_flagged_session(pool: &SqlitePool) -> AppResult<(String, i64)> {
    let policy = SessionPolicy::new(TEST_TTL_HOURS, false);
    let now = crate::services::identity::Clock::now(&SystemClock);
    let users = SqliteUserRepository::new(pool.clone());
    let sessions = SqliteSessionRepository::new(pool.clone());
    let user = users
        .create(&NewUser {
            username: "flagged-admin".to_string(),
            display_name: "Flagged Admin".to_string(),
            password_hash: "placeholder-not-a-real-argon2-hash".to_string(),
            must_change_password: true,
        })
        .await?;
    let token = mint_token()?;
    let session = sessions
        .insert(&NewSession {
            token_hash: hash_token(&token),
            user_id: user.id,
            expires_at: policy.expires_at(now),
            last_seen_at: now,
            user_agent: None,
        })
        .await?;
    Ok((token, session.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::password::PasswordHasher;
    use crate::services::identity::{IdentityService, SystemClock, ThrottleConfig};
    use sqlx::sqlite::SqlitePoolOptions;

    type TestIdentity = IdentityService<
        SqliteUserRepository,
        SqliteSessionRepository,
        SqliteRoleRepository,
        SystemClock,
        PasswordHasher,
    >;

    async fn test_pool() -> SqlitePool {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// The plumbing is real, not decoration: after `seed_session` the fixed
    /// token resolves through the production token hashing and cookie parser,
    /// and a token that was never seeded does not.
    #[tokio::test]
    async fn seeded_session_resolves_through_the_production_path() {
        // The constant tracks the real cookie name, in both directions.
        assert!(test_cookie_name_matches_production());
        assert_eq!(
            TEST_COOKIE,
            format!("{SESSION_COOKIE}={TEST_SESSION_TOKEN}"),
            "TEST_COOKIE must track SESSION_COOKIE and TEST_SESSION_TOKEN"
        );
        let policy = SessionPolicy::new(TEST_TTL_HOURS, false);
        assert_eq!(
            policy.parse_cookie(TEST_COOKIE).as_deref(),
            Some(TEST_SESSION_TOKEN),
            "the header value must survive the production cookie parser"
        );

        let pool = test_pool().await;
        let session_id = seed_session(&pool).await.unwrap();
        let service = TestIdentity::new(
            SqliteUserRepository::new(pool.clone()),
            SqliteSessionRepository::new(pool.clone()),
            SqliteRoleRepository::new(pool.clone()),
            SystemClock,
            PasswordHasher::light(),
            policy,
            ThrottleConfig::default(),
        );

        let resolved = service
            .resolve_session(TEST_SESSION_TOKEN)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("seeded session {session_id} must resolve"));
        assert_eq!(resolved.user.username, TEST_USERNAME);
        assert!(resolved.user.is_active, "seeded user is active");
        // S5: the seeded principal holds the protected `admin` role — the same
        // grant the bootstrap path performs — so every department `Require<P>`
        // is satisfied for the shared fixture. Membership is read through the
        // real effective-permission resolution the middleware uses.
        let effective = service
            .effective_permissions(&SqlitePermissionRepository::new(pool.clone()), resolved.user.id)
            .await
            .unwrap();
        assert_eq!(
            effective.len(),
            23,
            "the seeded test principal must hold the whole catalog"
        );
        assert!(
            effective.contains(
                <crate::security::authz::InventoryRead as crate::security::authz::Permission>::CODE
            ),
            "the seeded principal must hold the department permissions"
        );
        // The resolved session row is part of the production read: the fields
        // S3's session list and revocation screens render (and the ones the
        // middleware's expiry check reads) survive the round-trip.
        assert_eq!(resolved.session.user_id, resolved.user.id);
        assert!(!resolved.session.token_hash.is_empty(), "the digest is stored, never the token");
        assert!(resolved.session.created_at > chrono::Utc::now().naive_utc() - chrono::Duration::minutes(1));
        assert!(resolved.session.user_agent.is_none());
        assert_eq!(resolved.session.id, session_id);
        assert!(resolved.session.revoked_at.is_none());

        assert!(
            service
                .resolve_session("something-else")
                .await
                .unwrap()
                .is_none(),
            "a token that was never seeded must not resolve"
        );
    }
}
