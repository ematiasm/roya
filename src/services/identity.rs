// M5 identity kernel (Slice S1a, RBAC core added by S2). IdentityService owns
// the credential and session lifecycle: bootstrap admin, login with an
// in-memory throttle, session resolution with sliding renewal, logout, password
// change and mass revocation. Invariants owned here: the login failure is
// always the same generic error (unknown user, wrong password and inactive user
// are indistinguishable), the unknown-username path still pays a full
// verification cost (dummy hash) so answer time does not leak existence,
// throttling happens before verification, session validity is decided in SQL,
// and the injected clock makes every time rule testable without sleeping.
// Routing, middleware and templates are S1b; the effective-permission read and
// the bootstrap's protected-role grant are S2.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{Duration, NaiveDateTime};

use crate::error::{AppError, AppResult};
use crate::models::{NewSession, NewUserRole, ResolvedSession, Session, User};
use crate::repositories::{
    PermissionRepository, RoleRepository, SessionRepository, UserRepository,
};
use crate::security::password::PasswordHashing;
use crate::security::session::{hash_token, mint_token, SessionPolicy};

/// The seeded administrator username.
pub const BOOTSTRAP_ADMIN_USERNAME: &str = "admin";
pub const BOOTSTRAP_ADMIN_DISPLAY_NAME: &str = "Admin";
/// The protected role the bootstrap administrator holds (AC1 + AC14: the
/// triggers' holder arithmetic is only meaningful once the seeded
/// administrator actually holds the role). Same machine name as the username,
/// different rows.
pub const BOOTSTRAP_ADMIN_ROLE_CODE: &str = "admin";
/// Fixed generic login failure. Never append details: the sameness is the AC.
pub const GENERIC_LOGIN_FAILURE: &str = "Usuario o contraseña incorrectos";
/// Cost-equivalent stand-in verified against when the username does not exist.
const DUMMY_CREDENTIAL: &str = "roya-identity-verification-dummy";
/// Minimum accepted new password length (`POST /password`, spec Rules).
const MIN_PASSWORD_LEN: usize = 12;
/// Maximum number of distinct usernames the throttle map tracks at once.
/// Entries only stay while they are load-bearing — an open cooldown window,
/// or a last failure within `ThrottleConfig::decay` — so the cap's worst
/// case (many distinct below-cooldown usernames) self-heals instead of
/// saturating the map until restart.
const THROTTLE_MAX_TRACKED_KEYS: usize = 1_024;
/// Usernames are 3-64 chars; longer login keys are never real users, so they
/// are refused (with the generic error) without growing the throttle map.
const MAX_LOGIN_KEY_LEN: usize = 128;

/// Injectable source of "now": production uses UTC, tests advance a fake, so
/// the throttle cooldown and the 30-minute renewal horizon need no sleeping.
pub trait Clock: Send + Sync {
    fn now(&self) -> NaiveDateTime;
}

/// Production clock: wall-clock UTC, naive (the schema stores naive ISO TEXT).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> NaiveDateTime {
        chrono::Utc::now().naive_utc()
    }
}

/// Login throttling configuration (`ROYA_LOGIN_THROTTLE_*`).
#[derive(Debug, Clone)]
pub struct ThrottleConfig {
    /// Consecutive failures for one username before the cooldown starts.
    pub max_failures: u32,
    /// How long every attempt for that username is refused before verification.
    pub cooldown: Duration,
    /// Maximum distinct usernames tracked; brand-new keys beyond it are not
    /// recorded (already-tracked keys keep their counter).
    pub max_tracked: usize,
    /// How long an entry keeps its failure memory after its last failure.
    /// This decay horizon is what prevents permanent saturation: without it
    /// a key with fewer failures than `max_failures` has no cooldown and is
    /// never dropped, so enough distinct usernames would fill the cap and
    /// block tracking of every new one forever. Chosen as 15 minutes rather
    /// than the cooldown length (60 s): decaying a partial entry right
    /// after the cooldown would let an attacker who pauses just past the
    /// cooldown between probes reset the accumulated count every time; 15
    /// minutes keeps accumulation meaningful while a saturated map frees
    /// its slots within a quarter hour of silence.
    pub decay: Duration,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            max_failures: 5,
            cooldown: Duration::seconds(60),
            max_tracked: THROTTLE_MAX_TRACKED_KEYS,
            decay: Duration::minutes(15),
        }
    }
}

#[derive(Debug, Default)]
struct ThrottleState {
    consecutive_failures: u32,
    cooldown_until: Option<NaiveDateTime>,
    /// Instant of the most recent recorded failure: the decay horizon is
    /// measured from activity, so a key that goes quiet frees its slot even
    /// though it never reached the cooldown.
    last_failure: Option<NaiveDateTime>,
}

/// What `bootstrap_admin` needs to set a credential, in either recovery or
/// creation mode.
struct BootstrapCredential {
    password_hash: String,
    generated_password: Option<String>,
    must_change: bool,
}

/// Result of `bootstrap_admin`: what was created, plus the generated password
/// (returned exactly once, for `main` to log) when no env password was set.
pub struct BootstrapOutcome {
    pub created: bool,
    pub user: Option<User>,
    /// Present only when a password was generated; never stored in plaintext.
    pub generated_password: Option<String>,
}

impl std::fmt::Debug for BootstrapOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The generated password must never appear in logs or test output.
        f.debug_struct("BootstrapOutcome")
            .field("created", &self.created)
            .field("user", &self.user)
            .field("generated_password", &self.generated_password.is_some())
            .finish()
    }
}

/// Result of `login`: the user, the raw token (for the `Set-Cookie` header) and
/// the session expiry (for the cookie's `Max-Age`).
pub struct LoginOutcome {
    pub user: User,
    /// Raw cookie token; only its sha256 is stored. Returned once.
    pub token: String,
    pub expires_at: NaiveDateTime,
}

impl std::fmt::Debug for LoginOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginOutcome")
            .field("user", &self.user)
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Clone)]
pub struct IdentityService<U, S, R, C, H>
where
    U: UserRepository,
    S: SessionRepository,
    R: RoleRepository,
    C: Clock,
    H: PasswordHashing,
{
    pub users: U,
    pub sessions: S,
    /// The identity department owns its tables: the role grants the bootstrap
    /// writes live here (S2).
    pub roles: R,
    pub clock: C,
    pub hasher: H,
    pub policy: SessionPolicy,
    pub throttle: ThrottleConfig,
    /// In-memory, per-process throttle state: keyed by lowercased username.
    /// Counter resets on restart — a written-down limitation, not a silent gap.
    attempts: Arc<Mutex<HashMap<String, ThrottleState>>>,
}

impl<U, S, R, C, H> IdentityService<U, S, R, C, H>
where
    U: UserRepository,
    S: SessionRepository,
    R: RoleRepository,
    C: Clock,
    H: PasswordHashing,
{
    pub fn new(
        users: U,
        sessions: S,
        roles: R,
        clock: C,
        hasher: H,
        policy: SessionPolicy,
        throttle: ThrottleConfig,
    ) -> Self {
        Self {
            users,
            sessions,
            roles,
            clock,
            hasher,
            policy,
            throttle,
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // -- throttle helpers ------------------------------------------------------

    fn lock_attempts(&self) -> MutexGuard<'_, HashMap<String, ThrottleState>> {
        // A poisoned lock still holds valid counts; recover instead of failing.
        match self.attempts.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// `true` when attempts for this username are refused before verification.
    /// A window that has expired resets the counter: the next failure starts
    /// counting from one again instead of instantly re-throttling.
    fn throttled(&self, key: &str, now: NaiveDateTime) -> bool {
        let mut map = self.lock_attempts();
        if let Some(state) = map.get_mut(key) {
            if let Some(until) = state.cooldown_until {
                if now < until {
                    return true;
                }
                // The window expired: reset so the next failure starts
                // counting from one again instead of instantly re-throttling.
                map.remove(key);
            }
            // No cooldown yet (fewer than max failures): the counter stays.
        }
        false
    }

    fn record_failure(&self, key: &str, now: NaiveDateTime) {
        let mut map = self.lock_attempts();
        // Decay purge before the cap. An entry stays only while it is
        // load-bearing: an open cooldown window (the protection itself, so
        // it can never be dropped or evicted while it lasts), or a
        // below-cooldown entry whose last failure is still within the decay
        // horizon (so failures still accumulate across pauses shorter than
        // the horizon). An entry whose cooldown window already expired is
        // dropped outright: its protection is over and the next lookup
        // resets its counter anyway, so keeping it would be dead weight that
        // only delays the cap freeing up.
        map.retain(|_, state| match state.cooldown_until {
            Some(until) => now < until,
            None => match state.last_failure {
                Some(last) => now - last <= self.throttle.decay,
                None => false,
            },
        });
        if map.len() >= self.throttle.max_tracked && !map.contains_key(key) {
            // At the cap: a brand-new key is not tracked (it keeps failing
            // with the generic error, just without throttle memory). An
            // already-tracked key falls through and keeps its counter, so a
            // live cooldown for a real username can never be evicted.
            //
            // Residual tradeoff, stated honestly: while the map stays
            // saturated, a username that has never been attempted is not
            // tracked, so throttling protection for it is temporarily
            // absent. Saturation cannot last (every quiet entry decays
            // within `decay`), the cap bounds the map's cardinality, and an
            // unknown username already costs a full-cost argon2 hash per
            // attempt — but a cooldown-free attack on a fresh username is
            // possible while the map is full. Say no more than that.
            return;
        }
        let state = map.entry(key.to_string()).or_default();
        state.consecutive_failures += 1;
        state.last_failure = Some(now);
        if state.consecutive_failures >= self.throttle.max_failures {
            state.cooldown_until = Some(now + self.throttle.cooldown);
        }
    }

    fn clear_attempts(&self, key: &str) {
        self.lock_attempts().remove(key);
    }


    // -- bootstrap (AC1) --------------------------------------------------------

    /// Seed the admin when no active admin exists. `env_password` is used as-is
    /// (flag stays 0); with `None` a password is generated, returned once, and
    /// `must_change_password` confines the first session to the change route.
    /// The contract is "an administrator exists and can log in": an existing
    /// but inactive `admin` is reactivated with the new password, while an
    /// active one keeps the no-op behaviour.
    pub async fn bootstrap_admin(&self, env_password: Option<&str>) -> AppResult<BootstrapOutcome> {
        // The gate is the real quantity (S2): an ACTIVE user holding the
        // protected role. On an S1a-era database the administrator exists but
        // predates `user_roles`, so the count is zero and the bootstrap below
        // completes the seeding by granting the role.
        // The bootstrap's "is there an administrator?" decision reads the
        // same quantity the triggers protect: active holders of ANY
        // `is_system` role. A second protected role therefore satisfies it
        // too — the friendly pre-check cannot disagree with the real guard
        // (see `count_active_protected_holders`).
        if self.roles.count_active_protected_holders().await? > 0 {
            return Ok(BootstrapOutcome {
                created: false,
                user: None,
                generated_password: None,
            });
        }
        if let Some(existing) = self
            .users
            .find_with_hash_by_username(BOOTSTRAP_ADMIN_USERNAME)
            .await?
        {
            if existing.user.is_active {
                // Upgraded deployment: the S1a administrator is active but
                // predates the roles join, so the count above is zero and the
                // seeding is incomplete. Grant the protected role and touch
                // NOTHING else: the stored credential keeps verifying, and a
                // reset here would lock a working operator out on upgrade.
                self.grant_protected_role(existing.user.id).await?;
                let user = self
                    .users
                    .find_by_id(existing.user.id)
                    .await?
                    .unwrap_or(existing.user);
                return Ok(BootstrapOutcome {
                    created: true,
                    user: Some(user),
                    generated_password: None,
                });
            }
            // The username is taken by a deactivated account; creating it
            // again would only collide with the NOCASE unique index. Recover
            // instead: reactivate and set the new credential.
            let id = existing.user.id;
            let credential = self.bootstrap_credential(env_password)?;
            self.users.update_password_hash(id, &credential.password_hash).await?;
            self.users.set_must_change_password(id, credential.must_change).await?;
            self.users.set_active(id, true).await?;
            self.grant_protected_role(id).await?;
            let user = self
                .users
                .find_by_id(id)
                .await?
                .unwrap_or(existing.user);
            return Ok(BootstrapOutcome {
                created: true,
                user: Some(user),
                generated_password: credential.generated_password,
            });
        }
        let credential = self.bootstrap_credential(env_password)?;
        let user = self
            .users
            .create(&crate::models::NewUser {
                username: BOOTSTRAP_ADMIN_USERNAME.into(),
                display_name: BOOTSTRAP_ADMIN_DISPLAY_NAME.into(),
                password_hash: credential.password_hash,
                must_change_password: credential.must_change,
            })
            .await?;
        self.grant_protected_role(user.id).await?;
        Ok(BootstrapOutcome {
            created: true,
            user: Some(user),
            generated_password: credential.generated_password,
        })
    }

    /// Grant the protected role to the bootstrap administrator: the write the
    /// schema demands (`granted_by`/`granted_at`), idempotent through the
    /// grant's ON CONFLICT DO NOTHING, so a re-run cannot duplicate the row.
    async fn grant_protected_role(&self, user_id: i64) -> AppResult<()> {
        let admin = self
            .roles
            .find_by_code(BOOTSTRAP_ADMIN_ROLE_CODE)
            .await?
            .ok_or_else(|| {
                AppError::Internal(
                    "the seeded protected role is missing: run the migrations first".to_string(),
                )
            })?;
        self.roles
            .grant(&NewUserRole {
                user_id,
                role_id: admin.id,
                granted_by: user_id,
            })
            .await
    }

    /// Hash the bootstrap password: the env password is used as-is, or one is
    /// generated (returned once) with `must_change_password` set.
    fn bootstrap_credential(&self, env_password: Option<&str>) -> AppResult<BootstrapCredential> {
        match env_password {
            Some(password) => Ok(BootstrapCredential {
                password_hash: self.hasher.hash(password)?,
                generated_password: None,
                must_change: false,
            }),
            None => {
                // Same 32-byte url-safe shape as a session token: unguessable
                // and free of homoglyphs, so it can be typed from the log once.
                let generated = mint_token()?;
                Ok(BootstrapCredential {
                    password_hash: self.hasher.hash(&generated)?,
                    generated_password: Some(generated),
                    must_change: true,
                })
            }
        }
    }

    // -- login (AC4, AC5, AC6) ---------------------------------------------------

    /// Verify credentials and mint a fresh session. Every failure path returns
    /// the same `Unauthorized` message: the throttle check runs first, the
    /// unknown-username path still performs a full-cost verification, and a
    /// successful login clears the failure counter.
    pub async fn login(&self, username: &str, password: &str) -> AppResult<LoginOutcome> {
        let key = username.trim().to_lowercase();
        let now = self.clock.now();
        if self.throttled(&key, now) {
            return Err(AppError::Unauthorized(GENERIC_LOGIN_FAILURE.into()));
        }
        // Overlong keys cannot be usernames (schema CHECK); refuse with the
        // generic error and without recording throttle state for them.
        if key.len() > MAX_LOGIN_KEY_LEN {
            return Err(AppError::Unauthorized(GENERIC_LOGIN_FAILURE.into()));
        }

        let user_with_hash = self.users.find_with_hash_by_username(&key).await?;
        match user_with_hash {
            None => {
                // No stored hash to verify against: burn the same argon2 cost
                // on a dummy credential so timing does not leak existence.
                self.hasher.hash(DUMMY_CREDENTIAL)?;
                self.record_failure(&key, now);
                Err(AppError::Unauthorized(GENERIC_LOGIN_FAILURE.into()))
            }
            Some(candidate) => {
                if !self.hasher.verify(password, &candidate.password_hash) {
                    self.record_failure(&key, now);
                    return Err(AppError::Unauthorized(GENERIC_LOGIN_FAILURE.into()));
                }
                if !candidate.user.is_active {
                    // Correct password, deactivated account: still the same
                    // generic failure, still counted.
                    self.record_failure(&key, now);
                    return Err(AppError::Unauthorized(GENERIC_LOGIN_FAILURE.into()));
                }
                self.accept_login(candidate.user, now).await
            }
        }
    }

    async fn accept_login(&self, user: User, now: NaiveDateTime) -> AppResult<LoginOutcome> {
        // Mint and insert first, then touch `last_login_at`: a failed session
        // insert must not leave a half-done login stamped on the user.
        let token = mint_token()?;
        let expires_at = self.policy.expires_at(now);
        let session = self
            .sessions
            .insert(&NewSession {
                token_hash: hash_token(&token),
                user_id: user.id,
                expires_at,
                last_seen_at: now,
                // The web/API layer passes the user agent in S1b; the service
                // records none rather than inventing a placeholder.
                user_agent: None,
            })
            .await?;
        self.users.touch_last_login(user.id, now).await?;
        let user = self
            .users
            .find_by_id(user.id)
            .await?
            .unwrap_or(user);
        self.clear_attempts(&user.username.to_lowercase());
        Ok(LoginOutcome {
            user,
            token,
            expires_at: session.expires_at,
        })
    }

    // -- session resolution (AC7, AC8) --------------------------------------------

    /// Resolve a live session from a raw cookie token. Expired, revoked and
    /// inactive-owner sessions answer `None` exactly like an absent one (the
    /// SQL decides). When the session has idled past the renewal horizon, one
    /// UPDATE extends `last_seen_at` and `expires_at` together.
    pub async fn resolve_session(&self, token: &str) -> AppResult<Option<ResolvedSession>> {
        let token_hash = hash_token(token);
        let now = self.clock.now();
        let Some((mut session, user)) = self.sessions.resolve_valid(&token_hash, now).await?
        else {
            return Ok(None);
        };
        if self.policy.renewal_due(session.last_seen_at, now) {
            let expires_at = self.policy.expires_at(now);
            self.sessions.renew(&token_hash, now, expires_at).await?;
            session.last_seen_at = now;
            session.expires_at = expires_at;
        }
        Ok(Some(ResolvedSession { user, session }))
    }

    // -- effective permissions (AC11, Slice S2) -----------------------------------

    /// The union of the permission codes of every role the user holds
    /// (AC11), resolved in ONE query. No cache: a matrix edit applies to the
    /// next request, and a user with no roles resolves to the empty set.
    /// The permission repository rides in as an argument so the service's
    /// constructor — and with it the shared test-support construction path —
    /// stays untouched: the wiring owns the repositories, the service owns
    /// the read that answers "may this user do this?".
    pub async fn effective_permissions<P: PermissionRepository>(
        &self,
        permissions: &P,
        user_id: i64,
    ) -> AppResult<std::collections::BTreeSet<String>> {
        let codes = permissions.effective_for_user(user_id).await?;
        Ok(codes.into_iter().collect())
    }

    // -- logout (AC9) ---------------------------------------------------------------

    /// Revoke the session behind a token. Unknown/expired/already-revoked
    /// tokens are a no-op success: logout must always work.
    pub async fn logout(&self, token: &str) -> AppResult<()> {
        self.sessions.revoke(&hash_token(token)).await?;
        Ok(())
    }

    // -- password change (AC1/AC16, slice S3 part 1) ------------------------------

    /// Change a user's password: the current one is verified, the new one is
    /// validated (>= 12 chars, different from the current), the stored hash is
    /// replaced and `must_change_password` is cleared. Nothing is written when
    /// the current password does not verify: the validation runs before the
    /// first write.
    pub async fn change_password(
        &self,
        user_id: i64,
        current_password: &str,
        new_password: &str,
    ) -> AppResult<User> {
        let with_hash = self
            .users
            .find_with_hash_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("user {user_id} not found")))?;
        if !self.hasher.verify(current_password, &with_hash.password_hash) {
            return Err(AppError::Unauthorized(GENERIC_LOGIN_FAILURE.into()));
        }
        if new_password.chars().count() < MIN_PASSWORD_LEN {
            return Err(AppError::Validation(
                "La nueva contraseña debe tener al menos 12 caracteres.".into(),
            ));
        }
        if new_password == current_password {
            return Err(AppError::Validation(
                "La nueva contraseña debe ser distinta de la actual.".into(),
            ));
        }
        let new_hash = self.hasher.hash(new_password)?;
        self.users.update_password_hash(user_id, &new_hash).await?;
        self.users.set_must_change_password(user_id, false).await?;
        self.users
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("user {user_id} not found")))
    }

    /// The password change the confined flow performs (AC16): the change
    /// above — verify, validate, update the hash, clear the flag — followed
    /// by the revocation of every session the user holds EXCEPT the one the
    /// operator is acting through. The repository's
    /// `revoke_all_for_user_except` expresses the rule in one statement: no
    /// window in which the acting cookie names no row, no delete/insert, no
    /// Rust-bound timestamp compared against a database-written one, and the
    /// acting session keeps its id and its expiry.
    pub async fn change_password_keep_only_session(
        &self,
        keep_session: &Session,
        current_password: &str,
        new_password: &str,
    ) -> AppResult<()> {
        let user_id = keep_session.user_id;
        self.change_password(user_id, current_password, new_password)
            .await?;
        self.sessions
            .revoke_all_for_user_except(user_id, &keep_session.token_hash)
            .await?;
        Ok(())
    }

    // -- mass revocation -----------------------------------------------------------------

    /// Revoke every live session of a user (password change, deactivation).
    pub async fn revoke_all_sessions(&self, user_id: i64) -> AppResult<u64> {
        self.sessions.revoke_all_for_user(user_id).await
    }

    /// Opportunistic pruning of expired and revoked session rows. Returns how
    /// many rows went away; called where the app already touches the session
    /// store (no background task exists in this application).
    pub async fn prune_sessions(&self) -> AppResult<u64> {
        self.sessions.prune(self.clock.now()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NewUser;
    use crate::models::Session;
    use crate::repositories::{
        RoleRepository, SqlitePermissionRepository, SqliteRoleRepository, SqliteSessionRepository,
        SqliteUserRepository,
    };
    use crate::security::authz::PERMISSIONS;
    use crate::security::password::PasswordHasher;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use chrono::NaiveDate;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::{Row, SqlitePool};
    use std::sync::atomic::{AtomicUsize, Ordering};

    type TestIdentity = IdentityService<
        SqliteUserRepository,
        SqliteSessionRepository,
        SqliteRoleRepository,
        FakeClock,
        PasswordHasher,
    >;

    // -- test doubles --------------------------------------------------------

    #[derive(Clone)]
    struct FakeClock {
        current: Arc<Mutex<NaiveDateTime>>,
    }

    impl FakeClock {
        fn new(start: NaiveDateTime) -> Self {
            Self {
                current: Arc::new(Mutex::new(start)),
            }
        }

        fn advance(&self, d: Duration) {
            *self.current.lock().unwrap() += d;
        }

        fn now_value(&self) -> NaiveDateTime {
            *self.current.lock().unwrap()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> NaiveDateTime {
            *self.current.lock().unwrap()
        }
    }

    /// Counting wrapper: the honest spy for AC4 (a verification happens on the
    /// unknown-username path) and AC5 (the throttled attempt never verifies).
    #[derive(Clone)]
    struct SpyHasher {
        inner: PasswordHasher,
        hash_calls: Arc<AtomicUsize>,
        verify_calls: Arc<AtomicUsize>,
    }

    impl SpyHasher {
        fn new() -> Self {
            Self {
                inner: PasswordHasher::light(),
                hash_calls: Arc::new(AtomicUsize::new(0)),
                verify_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl PasswordHashing for SpyHasher {
        fn hash(&self, password: &str) -> AppResult<String> {
            self.hash_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.hash(password)
        }

        fn verify(&self, password: &str, stored_hash: &str) -> bool {
            self.verify_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.verify(password, stored_hash)
        }
    }

    /// N3: repository double whose inserts always fail — the deterministic
    /// stand-in for a rejected session write (e.g. a UNIQUE collision), so
    /// the insert-vs-touch ordering inside `accept_login` is observable.
    struct FailingInsertSessions;

    #[async_trait::async_trait]
    impl SessionRepository for FailingInsertSessions {
        async fn insert(&self, _input: &NewSession) -> AppResult<Session> {
            Err(AppError::Conflict("forced insert failure".into()))
        }
        async fn resolve_valid(
            &self,
            _token_hash: &str,
            _now: NaiveDateTime,
        ) -> AppResult<Option<(Session, User)>> {
            Ok(None)
        }
        async fn renew(
            &self,
            _token_hash: &str,
            _last_seen_at: NaiveDateTime,
            _expires_at: NaiveDateTime,
        ) -> AppResult<()> {
            Ok(())
        }
        async fn revoke(&self, _token_hash: &str) -> AppResult<bool> {
            Ok(false)
        }
        async fn revoke_all_for_user(&self, _user_id: i64) -> AppResult<u64> {
            Ok(0)
        }
        async fn revoke_all_for_user_except(
            &self,
            _user_id: i64,
            _keep_token_hash: &str,
        ) -> AppResult<u64> {
            Ok(0)
        }
        async fn prune(&self, _now: NaiveDateTime) -> AppResult<u64> {
            Ok(0)
        }
    }

    // -- fixtures --------------------------------------------------------------

    fn base_time() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 5, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

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

    async fn svc() -> (TestIdentity, SqlitePool, FakeClock) {
        let pool = test_pool().await;
        let clock = FakeClock::new(base_time());
        let policy = SessionPolicy::new(12, false);
        let service = IdentityService::new(
            SqliteUserRepository::new(pool.clone()),
            SqliteSessionRepository::new(pool.clone()),
            SqliteRoleRepository::new(pool.clone()),
            clock.clone(),
            PasswordHasher::light(),
            policy,
            ThrottleConfig::default(),
        );
        (service, pool, clock)
    }

    async fn spy_svc() -> (IdentityService<SqliteUserRepository, SqliteSessionRepository, SqliteRoleRepository, FakeClock, SpyHasher>, SqlitePool, FakeClock, SpyHasher) {
        let pool = test_pool().await;
        let clock = FakeClock::new(base_time());
        let hasher = SpyHasher::new();
        let service = IdentityService::new(
            SqliteUserRepository::new(pool.clone()),
            SqliteSessionRepository::new(pool.clone()),
            SqliteRoleRepository::new(pool.clone()),
            clock.clone(),
            hasher.clone(),
            SessionPolicy::new(12, false),
            ThrottleConfig::default(),
        );
        (service, pool, clock, hasher)
    }

    async fn seed_user<U, S, R, C, H>(
        s: &IdentityService<U, S, R, C, H>,
        username: &str,
        password: &str,
        active: bool,
    ) -> User
    where
        U: UserRepository,
        S: SessionRepository,
        R: RoleRepository,
        C: Clock,
        H: PasswordHashing,
    {
        let hash = s.hasher.hash(password).unwrap();
        let user = s
            .users
            .create(&NewUser {
                username: username.into(),
                display_name: username.into(),
                password_hash: hash,
                must_change_password: false,
            })
            .await
            .unwrap();
        if !active {
            s.users.set_active(user.id, false).await.unwrap();
        }
        user
    }

    fn unauthorized_text(err: &AppError) -> String {
        match err {
            AppError::Unauthorized(msg) => msg.clone(),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    async fn sessions_rows(pool: &SqlitePool) -> Vec<Session> {
        sqlx::query(
            "SELECT id, token_hash, user_id, created_at, expires_at, last_seen_at, revoked_at, user_agent FROM sessions ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| Session {
            id: row.get("id"),
            token_hash: row.get("token_hash"),
            user_id: row.get("user_id"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            last_seen_at: row.get("last_seen_at"),
            revoked_at: row.get("revoked_at"),
            user_agent: row.get("user_agent"),
        })
        .collect()
    }

    // -- AC1: bootstrap ---------------------------------------------------------

    #[tokio::test]
    async fn ac1_bootstrap_with_env_password_uses_it_and_needs_no_change() {
        let (s, _pool, _clock) = svc().await;
        let outcome = s.bootstrap_admin(Some("env password 123")).await.unwrap();
        assert!(outcome.created);
        assert!(outcome.generated_password.is_none());

        let stored = s
            .users
            .find_with_hash_by_username("ADMIN")
            .await
            .unwrap()
            .expect("admin must exist");
        assert!(stored.user.is_active);
        assert!(!stored.user.must_change_password);
        assert!(
            s.hasher.verify("env password 123", &stored.password_hash),
            "the env password must be the stored credential"
        );
    }

    #[tokio::test]
    async fn ac1_bootstrap_without_env_generates_once_and_flags_change() {
        let (s, _pool, _clock) = svc().await;
        let outcome = s.bootstrap_admin(None).await.unwrap();
        assert!(outcome.created);

        let generated = outcome
            .generated_password
            .as_deref()
            .expect("generated password returned once");
        assert!(!generated.is_empty());
        assert_ne!(generated, "");

        let stored = s
            .users
            .find_with_hash_by_username("admin")
            .await
            .unwrap()
            .expect("admin must exist");
        assert!(stored.user.must_change_password);
        // The generated password verifies against the stored PHC, but the
        // plaintext itself is stored nowhere.
        assert!(s.hasher.verify(generated, &stored.password_hash));
        assert_ne!(stored.password_hash, generated);
    }

    #[tokio::test]
    async fn ac1_second_bootstrap_creates_nothing() {
        let (s, _pool, _clock) = svc().await;
        let first = s.bootstrap_admin(Some("first password 1")).await.unwrap();
        assert!(first.created);

        let second = s.bootstrap_admin(Some("different password")).await.unwrap();
        assert!(!second.created, "a second bootstrap must not run");
        assert!(second.user.is_none());

        let count = s.roles.count_active_protected_holders().await.unwrap();
        assert_eq!(count, 1);

        // The original credential still verifies: nothing was overwritten.
        let stored = s
            .users
            .find_with_hash_by_username("admin")
            .await
            .unwrap()
            .unwrap();
        assert!(s.hasher.verify("first password 1", &stored.password_hash));
    }

    /// An S1a-era database state the S2 upgrade can encounter: the
    /// administrator row exists, deactivated, and `user_roles` is empty. Raw
    /// SQL on purpose — this state predates the grant path, and the S2
    /// triggers refuse to reach it through sanctioned writes (role_repo's
    /// tests prove those refusals).
    async fn seed_inactive_admin(pool: &SqlitePool) -> i64 {
        sqlx::query(
            r#"INSERT INTO users (username, display_name, password_hash, must_change_password, is_active)
               VALUES ('admin', 'Admin', 'placeholder-not-a-real-argon2-hash', 0, 0)"#,
        )
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    #[tokio::test]
    async fn ac1_bootstrap_recovers_an_inactive_admin() {
        let (s, pool, _clock) = svc().await;
        let admin_id = seed_inactive_admin(&pool).await;
        assert_eq!(s.roles.count_active_protected_holders().await.unwrap(), 0);

        let recovery = s.bootstrap_admin(Some("second password 2")).await.unwrap();
        assert!(recovery.created, "the inactive admin must be recovered, not collided");
        assert!(recovery.generated_password.is_none());
        let stored = s
            .users
            .find_with_hash_by_username("admin")
            .await
            .unwrap()
            .expect("the admin row must exist");
        assert!(stored.user.is_active);
        assert!(!stored.user.must_change_password);
        assert!(s.hasher.verify("second password 2", &stored.password_hash));
        assert!(!s.hasher.verify("first password 1", &stored.password_hash));
        // Exactly one admin row: the recovery reactivated it, never re-created.
        assert_eq!(s.roles.count_active_protected_holders().await.unwrap(), 1);
        // D2: the recovered administrator also ends up holding the role.
        assert_eq!(held_role_codes(&s, admin_id).await, vec!["admin"]);
    }

    #[tokio::test]
    async fn ac1_bootstrap_generated_password_also_recovers_an_inactive_admin() {
        let (s, pool, _clock) = svc().await;
        let admin_id = seed_inactive_admin(&pool).await;

        let recovery = s.bootstrap_admin(None).await.unwrap();
        assert!(recovery.created);
        let generated = recovery.generated_password.clone().unwrap();
        let stored = s.users.find_with_hash_by_username("admin").await.unwrap().unwrap();
        assert!(stored.user.is_active);
        assert!(stored.user.must_change_password, "generated recovery flags the change");
        assert!(s.hasher.verify(&generated, &stored.password_hash));
        // D2: the generated-password recovery also ends with the role granted.
        assert_eq!(held_role_codes(&s, admin_id).await, vec!["admin"]);
    }

    /// The role codes one user holds, through the real roles join the
    /// effective-permission read uses.
    async fn held_role_codes<U, S, R, C, H>(
        s: &IdentityService<U, S, R, C, H>,
        user_id: i64,
    ) -> Vec<String>
    where
        U: UserRepository,
        S: SessionRepository,
        R: RoleRepository,
        C: Clock,
        H: PasswordHashing,
    {
        s.roles
            .list_for_user(user_id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.code)
            .collect()
    }

    // -- AC14 (S2): the bootstrap administrator is a real protected-role holder ---

    #[tokio::test]
    async fn ac14_a_fresh_bootstrap_administrator_holds_exactly_the_catalog() {
        let (s, pool, _clock) = svc().await;
        let outcome = s.bootstrap_admin(Some("env password 123")).await.unwrap();
        assert!(outcome.created);
        let admin_id = outcome.user.unwrap().id;
        assert_eq!(held_role_codes(&s, admin_id).await, vec!["admin"]);

        // Through the roles join — the same one query the middleware resolves
        // per request — the administrator holds the whole 23-code catalog.
        let permissions = SqlitePermissionRepository::new(pool.clone());
        let mut codes = permissions.effective_for_user(admin_id).await.unwrap();
        codes.sort();
        let mut want: Vec<&str> = PERMISSIONS.to_vec();
        want.sort();
        assert_eq!(
            codes, want,
            "the bootstrap administrator must hold the whole seeded catalog"
        );
    }

    #[tokio::test]
    async fn ac14_the_trigger_now_protects_the_real_bootstrap_holder() {
        let (s, pool, _clock) = svc().await;
        let admin_id = s
            .bootstrap_admin(Some("env password 123"))
            .await
            .unwrap()
            .user
            .unwrap()
            .id;

        // Deactivating the administrator is refused by the schema trigger with
        // its own text — the S2 upgrade from the username shortcut is live.
        let err = sqlx::query("UPDATE users SET is_active = 0 WHERE id = ?")
            .bind(admin_id)
            .execute(&pool)
            .await
            .unwrap_err();
        match err {
            sqlx::Error::Database(db) => assert_eq!(
                db.message(),
                "cannot deactivate the last active user holding a protected role"
            ),
            other => panic!("expected the guard trigger, got {other:?}"),
        }

        // A second administrator makes the refused deactivation succeed ...
        let second = s
            .users
            .create(&NewUser {
                username: "second-admin".into(),
                display_name: "Second".into(),
                password_hash: "placeholder-not-a-real-argon2-hash".into(),
                must_change_password: false,
            })
            .await
            .unwrap();
        let admin_role = s.roles.find_by_code("admin").await.unwrap().unwrap();
        s.roles
            .grant(&NewUserRole {
                user_id: second.id,
                role_id: admin_role.id,
                granted_by: admin_id,
            })
            .await
            .unwrap();
        s.users.set_active(admin_id, false).await.unwrap();
        assert!(!s.users.find_by_id(admin_id).await.unwrap().unwrap().is_active);
        // ... and the first administrator's grant can now be removed too.
        s.roles.revoke(admin_id, admin_role.id).await.unwrap();
        assert!(held_role_codes(&s, admin_id).await.is_empty());
        // Exactly one active protected-role holder is left: the second admin.
        assert_eq!(s.roles.count_active_protected_holders().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn ac14_an_upgraded_active_administrator_gets_the_role_without_a_credential_reset() {
        // The other upgrade edge: an S1a administrator that is still active
        // and holds no role. The seeding completes by granting; the stored
        // credential keeps verifying.
        let (s, pool, _clock) = svc().await;
        let users = SqliteUserRepository::new(pool.clone());
        let created = users
            .create(&NewUser {
                username: "admin".into(),
                display_name: "Admin".into(),
                password_hash: "placeholder-not-a-real-argon2-hash".into(),
                must_change_password: false,
            })
            .await
            .unwrap();

        let outcome = s.bootstrap_admin(Some("whatever password")).await.unwrap();
        assert!(outcome.created, "the seeding completes the upgrade by granting");
        assert!(outcome.generated_password.is_none());
        // The credential was NOT reset: the placeholder hash survives, and the
        // bootstrap password must not verify against it.
        let stored = s.users.find_with_hash_by_username("admin").await.unwrap().unwrap();
        assert_eq!(stored.password_hash, "placeholder-not-a-real-argon2-hash");
        assert!(
            !s.hasher.verify("whatever password", &stored.password_hash),
            "bootstrap must not overwrite a live administrator's credential"
        );
        assert!(!stored.user.must_change_password);
        // The role landed, and the count the next bootstrap consults is 1.
        assert_eq!(held_role_codes(&s, created.id).await, vec!["admin"]);
        assert_eq!(s.roles.count_active_protected_holders().await.unwrap(), 1);
    }

    /// The service's "is there an administrator?" predicate and the guard
    /// triggers must agree on ANY `is_system` role, not only `admin`: with a
    /// second protected role in play, a holder of just that role satisfies
    /// the bootstrap and is protected by the same arithmetic.
    #[tokio::test]
    async fn ac14_the_protected_holder_predicate_agrees_with_the_triggers_across_two_protected_roles() {
        let (s, pool, _clock) = svc().await;
        // A second protected role, as an operator could create one by direct
        // SQL (the flag is writable at INSERT time; the trigger refuses to
        // flip it afterwards).
        sqlx::query(
            r#"INSERT INTO roles (code, name, description, is_system)
               VALUES ('dueno', 'Dueño', 'El dueño del local.', 1)"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // A user who holds ONLY the second protected role.
        let holder = seed_user(&s, "duena", "the right password", true).await;
        let dueno = s.roles.find_by_code("dueno").await.unwrap().unwrap();
        s.roles
            .grant(&NewUserRole {
                user_id: holder.id,
                role_id: dueno.id,
                granted_by: holder.id,
            })
            .await
            .unwrap();

        // The service counts them: the friendly pre-check sees an
        // administrator where the old code-keyed predicate saw zero.
        assert_eq!(
            s.roles.count_active_protected_holders().await.unwrap(),
            1,
            "the service must count holders of every protected role"
        );

        // ... and the triggers agree: refusing the last holder.
        let err = sqlx::query("UPDATE users SET is_active = 0 WHERE id = ?")
            .bind(holder.id)
            .execute(&pool)
            .await
            .unwrap_err();
        match err {
            sqlx::Error::Database(db) => assert_eq!(
                db.message(),
                "cannot deactivate the last active user holding a protected role"
            ),
            other => panic!("expected the guard trigger, got {other:?}"),
        }
        let err = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(holder.id)
            .execute(&pool)
            .await
            .unwrap_err();
        match err {
            sqlx::Error::Database(db) => assert_eq!(
                db.message(),
                "cannot delete the last active user holding a protected role"
            ),
            other => panic!("expected the guard trigger, got {other:?}"),
        }

        // ... and the bootstrap agrees too: with a protected-role holder
        // active, it seeds nothing.
        let second = s.bootstrap_admin(Some("whatever password")).await.unwrap();
        assert!(!second.created, "a protected-role holder is an administrator");

        // A second holder makes the same refusals lift — both protected roles
        // share the arithmetic.
        let partner = seed_user(&s, "socio", "the right password", true).await;
        s.roles
            .grant(&NewUserRole {
                user_id: partner.id,
                role_id: dueno.id,
                granted_by: holder.id,
            })
            .await
            .unwrap();
        s.users.set_active(holder.id, false).await.unwrap();
        assert_eq!(s.roles.count_active_protected_holders().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn ac1_bootstrap_with_active_admin_stays_a_noop() {
        let (s, _pool, _clock) = svc().await;
        s.bootstrap_admin(Some("first password 1")).await.unwrap();

        let second = s.bootstrap_admin(Some("different password")).await.unwrap();
        assert!(!second.created, "an active admin keeps the no-op");
        assert!(second.user.is_none());
        assert!(second.generated_password.is_none());
        let stored = s.users.find_with_hash_by_username("admin").await.unwrap().unwrap();
        assert!(s.hasher.verify("first password 1", &stored.password_hash));
    }

    // -- AC4: generic failure + verification on the unknown path ------------------

    #[tokio::test]
    async fn ac4_unknown_wrong_password_and_inactive_share_one_error() {
        let (s, _pool, _clock) = svc().await;
        let known = seed_user(&s, "teller", "known password 1", true).await;
        seed_user(&s, "fired", "fired password 1", false).await;

        let unknown = s.login("ghost", "whatever long password").await.unwrap_err();
        let wrong = s.login("teller", "wrong password 12").await.unwrap_err();
        let inactive = s.login("fired", "fired password 1").await.unwrap_err();

        let unknown_text = unauthorized_text(&unknown);
        assert_eq!(unknown_text, unauthorized_text(&wrong));
        assert_eq!(unknown_text, unauthorized_text(&inactive));

        // Sameness includes the mapped status, not only the body text.
        for err in [unknown, wrong, inactive] {
            let resp = err.into_response();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        // The known user still exists and its session machinery is untouched.
        assert!(s.users.find_by_id(known.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn ac4_unknown_username_still_costs_a_verification() {
        let (s, _pool, _clock, spy) = spy_svc().await;
        seed_user(&s, "teller", "known password 1", true).await;
        let before_hash = spy.hash_calls.load(Ordering::SeqCst);
        let before_verify = spy.verify_calls.load(Ordering::SeqCst);

        let err = s.login("ghost", "whatever long password").await.unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));

        // The unknown-username path performed full-cost hashing work (the
        // dummy verification), and it did not touch the stored credential.
        assert_eq!(
            spy.hash_calls.load(Ordering::SeqCst),
            before_hash + 1,
            "unknown username must still pay a verification cost"
        );
        assert_eq!(spy.verify_calls.load(Ordering::SeqCst), before_verify);

        // A known-username wrong password runs the real stored-hash verify.
        s.login("teller", "wrong password 12").await.unwrap_err();
        assert_eq!(spy.verify_calls.load(Ordering::SeqCst), before_verify + 1);
    }

    // -- AC5: throttling ------------------------------------------------------------

    async fn fail_n_times<U, S, R, C, H>(
        s: &IdentityService<U, S, R, C, H>,
        username: &str,
        password: &str,
        n: u32,
    ) where
        U: UserRepository,
        S: SessionRepository,
        R: RoleRepository,
        C: Clock,
        H: PasswordHashing,
    {
        for _ in 0..n {
            let err = s.login(username, password).await.unwrap_err();
            assert!(matches!(err, AppError::Unauthorized(_)));
        }
    }

    /// One failed login, asserting only the generic outcome (F8 helpers).
    async fn s_login_fail<U, S, R, C, H>(s: &IdentityService<U, S, R, C, H>, username: &str)
    where
        U: UserRepository,
        S: SessionRepository,
        R: RoleRepository,
        C: Clock,
        H: PasswordHashing,
    {
        let err = s.login(username, "whatever long password").await.unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn ac5_five_failures_throttle_the_sixth_attempt_before_verification() {
        let (s, _pool, clock, spy) = spy_svc().await;
        seed_user(&s, "teller", "the right password", true).await;

        fail_n_times(&s, "teller", "wrong password 12", 5).await;
        let verify_calls = spy.verify_calls.load(Ordering::SeqCst);
        assert_eq!(verify_calls, 5, "each failure verified the stored hash");

        // The sixth attempt carries the CORRECT password, yet is refused
        // before verification: verify_calls must not move.
        let err = s.login("teller", "the right password").await.unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));
        assert_eq!(
            spy.verify_calls.load(Ordering::SeqCst),
            verify_calls,
            "the throttled attempt must not reach verification"
        );

        // The cooldown expires with the injected clock; the correct password
        // then logs in without any new failure.
        clock.advance(Duration::seconds(61));
        let outcome = s.login("teller", "the right password").await.unwrap();
        assert_eq!(outcome.user.username, "teller");
    }

    #[tokio::test]
    async fn ac5_success_clears_the_counter() {
        let (s, _pool, _clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;

        // 4 failures (below the threshold of 5), then a successful login.
        fail_n_times(&s, "teller", "wrong password 12", 4).await;
        s.login("teller", "the right password").await.unwrap();

        // 4 more failures on top: 8 failures against the SAME username, but
        // only 4 since the success. If the success did not clear the counter,
        // the carried-over 4 would trip the cooldown here and the 9th
        // consecutive overall attempt (the correct password) would be
        // refused — with the clear, it must authenticate.
        fail_n_times(&s, "teller", "wrong password 12", 4).await;
        let outcome = s.login("teller", "the right password").await.unwrap();
        assert_eq!(outcome.user.username, "teller");
    }

    #[tokio::test]
    async fn ac5_throttle_is_case_insensitive_and_per_username() {
        let (s, _pool, _clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        seed_user(&s, "cashier", "cashier password", true).await;

        fail_n_times(&s, "TELLER", "wrong password 12", 5).await;
        // Same username in another casing is throttled too.
        let err = s.login("Teller", "the right password").await.unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));
        // A different username is unaffected.
        let ok = s.login("cashier", "cashier password").await.unwrap();
        assert_eq!(ok.user.username, "cashier");
    }

    // -- AC6: token hashing ----------------------------------------------------------

    #[tokio::test]
    async fn ac6_stored_hash_is_hash_token_and_raw_token_in_no_column() {
        let (s, pool, _clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        let outcome = s.login("teller", "the right password").await.unwrap();
        let token = outcome.token.clone();

        let rows = sessions_rows(&pool).await;
        assert_eq!(rows.len(), 1, "one login, one session row");
        let session = &rows[0];
        assert_eq!(session.token_hash, hash_token(&token));
        assert_ne!(session.token_hash, token);

        // The raw token must not appear in any column of the row.
        let full: (i64, String, i64, String, String, String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT id, token_hash, user_id, created_at, expires_at, last_seen_at, revoked_at, user_agent FROM sessions WHERE id = ?",
            )
            .bind(session.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        for value in [&full.1, &full.3, &full.4, &full.5] {
            assert!(!value.contains(&token), "raw token leaked into a column");
        }
        if let Some(revoked) = &full.6 {
            assert!(!revoked.contains(&token));
        }
        if let Some(agent) = &full.7 {
            assert!(!agent.contains(&token));
        }
    }

    // -- AC7: expiry, revocation, permanence ------------------------------------------

    #[tokio::test]
    async fn ac7_expired_session_refused_although_the_row_remains() {
        let (s, pool, clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        let outcome = s.login("teller", "the right password").await.unwrap();

        clock.advance(Duration::hours(12));
        let resolved = s.resolve_session(&outcome.token).await.unwrap();
        assert!(resolved.is_none(), "expired session must be refused");

        let rows = sessions_rows(&pool).await;
        assert_eq!(rows.len(), 1, "the expired row still exists");
        assert!(rows[0].revoked_at.is_none(), "expiry is not revocation");
    }

    #[tokio::test]
    async fn ac7_revoked_session_refused_and_revocation_is_idempotent() {
        let (s, _pool, _clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        let outcome = s.login("teller", "the right password").await.unwrap();

        s.logout(&outcome.token).await.unwrap();
        assert!(s.resolve_session(&outcome.token).await.unwrap().is_none());

        // Revoking twice is fine: the second call is a no-op success.
        s.logout(&outcome.token).await.unwrap();
        assert!(s.resolve_session(&outcome.token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ac7_revoked_at_cannot_be_cleared_at_the_sql_level() {
        let (s, pool, _clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        let outcome = s.login("teller", "the right password").await.unwrap();
        s.logout(&outcome.token).await.unwrap();

        let err = sqlx::query("UPDATE sessions SET revoked_at = NULL WHERE user_id = ?")
            .bind(outcome.user.id)
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("revoked_at cannot be cleared"),
            "the schema trigger must refuse it: {err}"
        );
    }

    #[tokio::test]
    async fn ac7_inactive_user_session_is_refused() {
        let (s, _pool, _clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        let outcome = s.login("teller", "the right password").await.unwrap();
        assert!(s.resolve_session(&outcome.token).await.unwrap().is_some());

        // Deactivate the owner: the live session must be refused from now on,
        // with exactly the outcome an unknown token gets.
        s.users.set_active(outcome.user.id, false).await.unwrap();
        let inactive = s.resolve_session(&outcome.token).await.unwrap();
        let unknown = s.resolve_session("no such token").await.unwrap();
        assert!(inactive.is_none(), "deactivated owner's session must be refused");
        assert_eq!(inactive.is_none(), unknown.is_none(), "must match the unknown-token outcome");
    }

    // -- AC8: sliding renewal -----------------------------------------------------------

    #[tokio::test]
    async fn ac8_renewal_after_30_idle_minutes_and_not_before() {
        let (s, _pool, clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        let outcome = s.login("teller", "the right password").await.unwrap();
        let login_at = clock.now_value();

        // Activity before the horizon: no renewal.
        clock.advance(Duration::minutes(29));
        let before = s.resolve_session(&outcome.token).await.unwrap().unwrap();
        assert_eq!(before.session.expires_at, login_at + Duration::hours(12));
        assert_eq!(before.session.last_seen_at, login_at);

        // Past the horizon: expiry extends to now + TTL.
        clock.advance(Duration::minutes(2));
        let renewed = s.resolve_session(&outcome.token).await.unwrap().unwrap();
        assert_eq!(renewed.session.last_seen_at, clock.now_value());
        assert_eq!(renewed.session.expires_at, clock.now_value() + Duration::hours(12));
    }

    // -- AC9: logout ----------------------------------------------------------------------

    #[tokio::test]
    async fn ac9_logout_with_unknown_token_is_noop_success() {
        let (s, _pool, _clock) = svc().await;
        s.logout("no such token").await.unwrap();

        // Also for a token that hashes to a nonexistent row.
        s.logout(mint_token().unwrap().as_str()).await.unwrap();
    }

    // -- password change -------------------------------------------------------------------

    #[tokio::test]
    async fn change_password_verifies_current_and_clears_the_flag() {
        let (s, _pool, _clock) = svc().await;
        let outcome = s.bootstrap_admin(None).await.unwrap();
        let user = outcome.user.clone().unwrap();
        let generated = outcome.generated_password.clone().unwrap();

        // Wrong current password is refused.
        let err = s
            .change_password(user.id, "not the password", "brand new password")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));

        let changed = s
            .change_password(user.id, &generated, "brand new password")
            .await
            .unwrap();
        assert!(!changed.must_change_password);

        let stored = s
            .users
            .find_with_hash_by_id(user.id)
            .await
            .unwrap()
            .unwrap();
        assert!(s.hasher.verify("brand new password", &stored.password_hash));
        assert!(!s.hasher.verify(&generated, &stored.password_hash));
    }

    #[tokio::test]
    async fn change_password_rejects_short_and_repeated_passwords() {
        let (s, _pool, _clock) = svc().await;
        seed_user(&s, "teller", "current password", true).await;

        let err = s
            .change_password(1, "current password", "short")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let err = s
            .change_password(1, "current password", "current password")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn revoke_all_sessions_kills_every_live_session_of_the_user() {
        let (s, pool, _clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        let first = s.login("teller", "the right password").await.unwrap();
        let second = s.login("teller", "the right password").await.unwrap();
        assert_eq!(sessions_rows(&pool).await.len(), 2);

        let revoked = s.revoke_all_sessions(first.user.id).await.unwrap();
        assert_eq!(revoked, 2);
        assert!(s.resolve_session(&first.token).await.unwrap().is_none());
        assert!(s.resolve_session(&second.token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_confined_change_clears_the_flag_kills_the_other_sessions_and_keeps_the_actor() {
        let (s, _pool, _clock) = svc().await;
        let boot = s.bootstrap_admin(None).await.unwrap();
        let generated = boot.generated_password.clone().unwrap();
        let user_id = boot.user.clone().unwrap().id;

        // Two sessions for the same flagged user, like two open tabs.
        let acting = s.login("admin", &generated).await.unwrap();
        let other = s.login("admin", &generated).await.unwrap();
        assert!(s.resolve_session(&other.token).await.unwrap().is_some());

        let acting_resolved = s
            .resolve_session(&acting.token)
            .await
            .unwrap()
            .expect("the acting session must resolve before the change");

        s.change_password_keep_only_session(
            &acting_resolved.session,
            &generated,
            "una contraseña nueva larga",
        )
        .await
        .unwrap();

        // The acting session survives under the same cookie token — same row,
        // same id, same expiry — with the flag cleared...
        let kept = s
            .resolve_session(&acting.token)
            .await
            .unwrap()
            .expect("the acting session must survive its own password change");
        assert_eq!(kept.session.id, acting_resolved.session.id, "same row, same id");
        assert_eq!(kept.session.expires_at, acting_resolved.session.expires_at);
        assert!(!kept.user.must_change_password, "the flag must be cleared");
        // ... the other session of the same user is dead ...
        assert!(s.resolve_session(&other.token).await.unwrap().is_none());
        // ... and the stored hash verifies the NEW password only.
        let stored = s.users.find_with_hash_by_id(user_id).await.unwrap().unwrap();
        assert!(s.hasher.verify("una contraseña nueva larga", &stored.password_hash));
        assert!(!s.hasher.verify(&generated, &stored.password_hash));
    }

    #[tokio::test]
    async fn a_wrong_current_password_in_the_confined_change_writes_nothing() {
        let (s, _pool, _clock) = svc().await;
        let boot = s.bootstrap_admin(None).await.unwrap();
        let generated = boot.generated_password.clone().unwrap();
        let acting = s.login("admin", &generated).await.unwrap();
        let other = s.login("admin", &generated).await.unwrap();
        let resolved = s.resolve_session(&acting.token).await.unwrap().unwrap();

        let err = s
            .change_password_keep_only_session(
                &resolved.session,
                "not the password at all",
                "una contraseña nueva larga",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));

        // Nothing moved: both sessions still resolve and the flag still holds.
        assert!(s.resolve_session(&acting.token).await.unwrap().is_some());
        assert!(s.resolve_session(&other.token).await.unwrap().is_some());
        let stored = s.users.find_with_hash_by_id(acting.user.id).await.unwrap().unwrap();
        assert!(stored.user.must_change_password);
        assert!(s.hasher.verify(&generated, &stored.password_hash));
    }

    // -- pruning -----------------------------------------------------------------------------

    #[tokio::test]
    async fn prune_removes_expired_and_revoked_rows_and_keeps_live_ones() {
        let (s, pool, clock) = svc().await;
        seed_user(&s, "teller", "the right password", true).await;
        let live = s.login("teller", "the right password").await.unwrap();
        let doomed = s.login("teller", "the right password").await.unwrap();
        let revoked = s.login("teller", "the right password").await.unwrap();
        s.logout(&revoked.token).await.unwrap();
        // Re-stamp `revoked_at` in the database's own canonical form at the
        // fake-clock instant: the production logout writes real wall-clock
        // `strftime('now')`, which the injected clock cannot freeze.
        let revoked_at_instant = clock.now_value();
        sqlx::query("UPDATE sessions SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', ?) WHERE token_hash = ?")
            .bind(revoked_at_instant.format("%Y-%m-%d %H:%M:%S").to_string())
            .bind(hash_token(&revoked.token))
            .execute(&pool)
            .await
            .unwrap();

        // Revoked but still unexpired: prune must remove it now — the trait
        // contract ships no retention window, and the expiry branch alone
        // cannot catch this row until its TTL passes.
        clock.advance(Duration::hours(1));
        let pruned = s.prune_sessions().await.unwrap();
        assert_eq!(pruned, 1, "the revoked-but-unexpired row is pruned");
        assert!(s.resolve_session(&revoked.token).await.unwrap().is_none());

        // Move far past the TTL: `live` and `doomed` are expired now. Logging
        // out the expired one again is still a no-op success (AC9 for rows
        // whose time has passed).
        clock.advance(Duration::hours(24));
        s.logout(&doomed.token).await.unwrap();

        let pruned = s.prune_sessions().await.unwrap();
        assert_eq!(pruned, 2, "the two expired rows");
        let rows = sessions_rows(&pool).await;
        assert_eq!(rows.len(), 0);
        assert!(s.resolve_session(&live.token).await.unwrap().is_none());
    }

    // -- AC23: nothing leaks -------------------------------------------------------------------

    #[tokio::test]
    async fn ac23_no_password_or_token_in_error_or_debug_output() {
        let (s, _pool, _clock) = svc().await;
        let password = "the right password";
        let wrong = "wrong password 12";
        seed_user(&s, "teller", password, true).await;

        // Login outcome: token redacted from Debug.
        let outcome = s.login("teller", password).await.unwrap();
        let debug = format!("{outcome:?}");
        assert!(!debug.contains(&outcome.token), "token leaked in Debug: {debug}");
        assert!(!debug.contains(password));

        // Bootstrap outcome: generated password redacted from Debug.
        let (fresh, _pool2, _clock2) = svc().await;
        let boot = fresh.bootstrap_admin(None).await.unwrap();
        let generated = boot.generated_password.clone().unwrap();
        let debug = format!("{boot:?}");
        assert!(!debug.contains(&generated), "generated password leaked: {debug}");

        // Error output (message and Debug) carries neither secret.
        let err = s.login("teller", wrong).await.unwrap_err();
        let rendered = format!("{err}");
        let rendered_debug = format!("{err:?}");
        for secret in [password, wrong] {
            assert!(!rendered.contains(secret), "password in error message");
            assert!(!rendered_debug.contains(secret), "password in error Debug");
        }

        // The stored PHC never serializes: UserWithHash has no Serialize impl,
        // and the plain User read carries no hash at all.
        let user = s.users.find_by_username("teller").await.unwrap().unwrap();
        let user_json = serde_json::to_string(&user).unwrap();
        assert!(!user_json.contains("password_hash") && !user_json.contains("$argon2"));

        // F5: the Debug of the credential-bearing structs redacts the stored
        // verifier — neither the PHC string nor the plaintext of the password
        // used to build them may appear.
        let with_hash = s.users.find_with_hash_by_username("teller").await.unwrap().unwrap();
        let with_hash_debug = format!("{with_hash:?}");
        assert!(
            !with_hash_debug.contains("$argon2"),
            "stored verifier leaked in UserWithHash Debug: {with_hash_debug}"
        );
        assert!(!with_hash_debug.contains(password));

        let new_user = crate::models::NewUser {
            username: "teller".into(),
            display_name: "Teller".into(),
            password_hash: with_hash.password_hash.clone(),
            must_change_password: false,
        };
        let new_user_debug = format!("{new_user:?}");
        assert!(
            !new_user_debug.contains("$argon2"),
            "stored verifier leaked in NewUser Debug: {new_user_debug}"
        );
        assert!(!new_user_debug.contains(password));
    }

    // -- F8: throttle map cardinality ------------------------------------------------

    /// Distinct unknown usernames: the cap is never exceeded, a brand-new key
    /// beyond it is not tracked, and an already-tracked username keeps its
    /// cooldown (it stays throttled while new keys are refused tracking).
    #[tokio::test]
    async fn f8_throttle_cap_never_exceeded_and_tracked_keys_survive() {
        let pool = test_pool().await;
        let clock = FakeClock::new(base_time());
        let hasher = SpyHasher::new();
        let service = IdentityService::new(
            SqliteUserRepository::new(pool.clone()),
            SqliteSessionRepository::new(pool.clone()),
            SqliteRoleRepository::new(pool.clone()),
            clock.clone(),
            hasher.clone(),
            SessionPolicy::new(12, false),
            // max_failures = 1: a single recorded failure starts a cooldown.
            ThrottleConfig {
                max_failures: 1,
                cooldown: Duration::seconds(60),
                max_tracked: 2,
                decay: Duration::minutes(15),
            },
        );

        // ghost1 and ghost2 fill the map to the cap (both cooling now).
        s_login_fail(&service, "ghost1").await;
        s_login_fail(&service, "ghost2").await;
        assert_eq!(service.attempts.lock().unwrap().len(), 2);

        // ghost3: a brand-new key beyond the cap — refused tracking.
        s_login_fail(&service, "ghost3").await;
        assert_eq!(
            service.attempts.lock().unwrap().len(),
            2,
            "the cap must not be exceeded"
        );
        // ghost3 was not tracked: its next attempt is NOT throttled, so the
        // unknown-username dummy verification runs (hash call moves).
        let before = hasher.hash_calls.load(Ordering::SeqCst);
        s_login_fail(&service, "ghost3").await;
        assert_eq!(
            hasher.hash_calls.load(Ordering::SeqCst),
            before + 1,
            "an untracked key must not be throttled"
        );

        // An already-tracked key keeps its counter even at the cap: ghost1
        // stays throttled (no verification work happens).
        let before = hasher.hash_calls.load(Ordering::SeqCst);
        s_login_fail(&service, "ghost1").await;
        assert_eq!(
            hasher.hash_calls.load(Ordering::SeqCst),
            before,
            "a tracked key must stay throttled while new keys are refused"
        );
    }

    /// Purging on record: once a cooldown window has expired, recording a new
    /// failure drops the dead entries and the cap admits new keys again.
    #[tokio::test]
    async fn f8_expired_windows_are_purged_so_the_cap_frees_up() {
        let pool = test_pool().await;
        let clock = FakeClock::new(base_time());
        let service = IdentityService::new(
            SqliteUserRepository::new(pool.clone()),
            SqliteSessionRepository::new(pool.clone()),
            SqliteRoleRepository::new(pool.clone()),
            clock.clone(),
            PasswordHasher::light(),
            SessionPolicy::new(12, false),
            ThrottleConfig {
                max_failures: 1,
                cooldown: Duration::seconds(60),
                max_tracked: 2,
                decay: Duration::minutes(15),
            },
        );

        s_login_fail(&service, "ghost1").await;
        s_login_fail(&service, "ghost2").await;
        s_login_fail(&service, "ghost3").await; // refused: at cap
        assert_eq!(service.attempts.lock().unwrap().len(), 2);

        clock.advance(Duration::seconds(61));
        s_login_fail(&service, "ghost4").await;
        // Both stale windows (ghost1, ghost2) expired at the same instant and
        // were purged on the next record; ghost4 now occupies one slot.
        assert_eq!(service.attempts.lock().unwrap().len(), 1);
        assert!(
            service.attempts.lock().unwrap().contains_key("ghost4"),
            "a new key must be tracked once a window expired"
        );
        assert!(
            !service.attempts.lock().unwrap().contains_key("ghost1"),
            "the expired window must have been purged"
        );
    }

    // -- N1: decay horizon keeps the cap from saturating ------------------------------

    fn decay_throttle() -> ThrottleConfig {
        // The shipped max_failures shape (5): below it an entry has NO
        // cooldown, which is exactly the shape that used to survive forever.
        // The small cap and short decay make the horizon observable fast.
        ThrottleConfig {
            max_failures: 5,
            cooldown: Duration::seconds(60),
            max_tracked: 2,
            decay: Duration::seconds(30),
        }
    }

    /// N1a: with the shipped `max_failures = 5`, a below-cooldown (partial)
    /// entry goes quiet and is purged after the decay horizon, so a key that
    /// was refused tracking at the cap becomes trackable again and the cap
    /// is never exceeded.
    #[tokio::test]
    async fn n1a_partial_entries_decay_so_a_refused_key_becomes_trackable_again() {
        let pool = test_pool().await;
        let clock = FakeClock::new(base_time());
        let service = IdentityService::new(
            SqliteUserRepository::new(pool.clone()),
            SqliteSessionRepository::new(pool.clone()),
            SqliteRoleRepository::new(pool.clone()),
            clock.clone(),
            PasswordHasher::light(),
            SessionPolicy::new(12, false),
            decay_throttle(),
        );

        // Two partial entries (1 failure each, below max_failures: no
        // cooldown) fill the map to the cap.
        s_login_fail(&service, "ghost1").await;
        s_login_fail(&service, "ghost2").await;
        assert_eq!(service.attempts.lock().unwrap().len(), 2);

        // ghost3: refused tracking at the cap.
        s_login_fail(&service, "ghost3").await;
        assert_eq!(
            service.attempts.lock().unwrap().len(),
            2,
            "the cap must not be exceeded"
        );
        assert!(
            !service.attempts.lock().unwrap().contains_key("ghost3"),
            "a fresh key at the cap must not be tracked"
        );

        // Both partial entries go quiet past the decay horizon (and no
        // cooldown is involved): the next record purges them, and the key
        // that was refused becomes trackable again.
        clock.advance(Duration::seconds(31));
        s_login_fail(&service, "ghost3").await;
        let map = service.attempts.lock().unwrap();
        assert_eq!(map.len(), 1, "the cap must still hold after the decay");
        assert!(
            map.contains_key("ghost3"),
            "a quiet horizon past key must free its slot for a new one"
        );
        assert!(!map.contains_key("ghost1") && !map.contains_key("ghost2"));
    }

    /// N1b: with the shipped `max_failures = 5`, a tracked username that
    /// reached the cooldown keeps its protection and cannot be evicted by a
    /// flood of distinct new keys while its window is open.
    #[tokio::test]
    async fn n1b_a_cooling_username_keeps_its_protection_against_a_flood_of_new_keys() {
        let pool = test_pool().await;
        let clock = FakeClock::new(base_time());
        let hasher = SpyHasher::new();
        let service = IdentityService::new(
            SqliteUserRepository::new(pool.clone()),
            SqliteSessionRepository::new(pool.clone()),
            SqliteRoleRepository::new(pool.clone()),
            clock.clone(),
            hasher.clone(),
            SessionPolicy::new(12, false),
            // Cap of 1: teller's slot is the only one, so any eviction or
            // cap-override during the flood is immediately observable.
            ThrottleConfig {
                max_failures: 5,
                cooldown: Duration::seconds(60),
                max_tracked: 1,
                decay: Duration::seconds(30),
            },
        );
        seed_user(&service, "teller", "the right password", true).await;

        // Five consecutive failures put teller into cooldown: the partial
        // counter must accumulate to the threshold while the entry has no
        // cooldown yet.
        fail_n_times(&service, "teller", "wrong password 12", 5).await;
        assert!(
            service.attempts.lock().unwrap().contains_key("teller"),
            "the cooling entry must be tracked"
        );

        // Flood distinct new keys while the window is open: none may evict
        // teller and the cap must hold.
        for i in 0..10 {
            s_login_fail(&service, &format!("flood{i}")).await;
        }
        assert_eq!(
            service.attempts.lock().unwrap().len(),
            1,
            "the cap must hold during the flood"
        );
        assert!(
            service.attempts.lock().unwrap().contains_key("teller"),
            "a cooling key cannot be evicted while its window is open"
        );

        // teller is still throttled: the CORRECT password is refused before
        // any verification happens.
        let before = hasher.verify_calls.load(Ordering::SeqCst);
        let err = service.login("teller", "the right password").await.unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));
        assert_eq!(
            hasher.verify_calls.load(Ordering::SeqCst),
            before,
            "the cooling username must stay throttled: no verification"
        );

        // After the window expires, the usual reset applies: protection is
        // temporary, not permanent, and the correct password logs in.
        clock.advance(Duration::seconds(61));
        let outcome = service.login("teller", "the right password").await.unwrap();
        assert_eq!(outcome.user.username, "teller");
    }

    // -- N3: last_login_at stamping and its ordering ------------------------------

    /// N3a: a successful login stamps `last_login_at` with the injected
    /// clock's instant — the assertion that pins the touch itself.
    #[tokio::test]
    async fn n3a_successful_login_stamps_last_login_at_with_the_clock_instant() {
        let (s, _pool, clock) = svc().await;
        let user = seed_user(&s, "teller", "the right password", true).await;
        assert_eq!(
            user.last_login_at, None,
            "precondition: a fresh user was never logged in"
        );

        let outcome = s.login("teller", "the right password").await.unwrap();
        assert_eq!(
            outcome.user.last_login_at,
            Some(clock.now_value()),
            "the successful login must stamp last_login_at at the login instant"
        );
    }

    /// N3b: `accept_login` inserts the session BEFORE touching
    /// `last_login_at` — a failed session insert must leave the user
    /// unstamped, so the ordering (insert first, touch second) is pinned.
    #[tokio::test]
    async fn n3b_a_failed_session_insert_never_stamps_last_login_at() {
        let pool = test_pool().await;
        let clock = FakeClock::new(base_time());
        let service = IdentityService::new(
            SqliteUserRepository::new(pool.clone()),
            FailingInsertSessions,
            SqliteRoleRepository::new(pool.clone()),
            clock.clone(),
            PasswordHasher::light(),
            SessionPolicy::new(12, false),
            ThrottleConfig::default(),
        );
        let user = seed_user(&service, "teller", "the right password", true).await;
        assert_eq!(user.last_login_at, None, "precondition");

        // Credentials are correct; only the session insert fails.
        let err = service
            .login("teller", "the right password")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");

        let after = service
            .users
            .find_by_id(user.id)
            .await
            .unwrap()
            .expect("the user row must exist");
        assert_eq!(
            after.last_login_at, None,
            "a failed session insert must not advance last_login_at: the touch runs after the insert"
        );
    }
}
