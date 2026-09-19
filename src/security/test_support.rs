// Test-only identity support (part 1 of the S1b slice split). Every HTTP test
// module authenticates with ONE fixed session token: `seed_session` inserts a
// real user plus a live session through the real repositories, and
// `with_cookie`/`TEST_COOKIE` put the token on the request. Nothing enforces
// anything yet (the deny-by-default middleware is part 2), so the cookie is
// sent and ignored today; part 2 flips enforcement on and every test is already
// authenticated, so no assertion changes.
//
// The design is deliberately constant-shaped: no test needs a new variable, no
// helper signature changes and no call site is rewritten beyond one header.
use axum::http::request::Builder;
use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::{NewSession, NewUser};
use crate::repositories::{
    SessionRepository, SqliteSessionRepository, SqliteUserRepository, UserRepository,
};
use crate::security::session::{hash_token, SessionPolicy, SESSION_COOKIE};

/// The one session token every HTTP test authenticates with. Fixed so the
/// cookie value can be a compile-time constant; only its sha256 digest is
/// stored, exactly like the production path.
pub const TEST_SESSION_TOKEN: &str = "test-session-token-roya-identity";

/// The `Cookie` header value carrying [`TEST_SESSION_TOKEN`]. Duplicated from
/// `SESSION_COOKIE` (a `const` cannot be `concat!`-built from another `const`),
/// with the drift guarded two ways below: a compile-time assertion and the
/// plumbing test's round-trip through the real cookie parser.
pub const TEST_COOKIE: &str = "roya_session=test-session-token-roya-identity";

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
pub async fn seed_session(pool: &SqlitePool) -> AppResult<i64> {
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

/// Add the test session cookie to a request builder, so the built request is
/// already authenticated for part 2's deny-by-default middleware.
pub fn with_cookie(builder: Builder) -> Builder {
    builder.header(axum::http::header::COOKIE, TEST_COOKIE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::password::PasswordHasher;
    use crate::services::identity::{IdentityService, SystemClock, ThrottleConfig};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    type TestIdentity = IdentityService<
        SqliteUserRepository,
        SqliteSessionRepository,
        SystemClock,
        PasswordHasher,
    >;

    async fn test_pool() -> SqlitePool {
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
