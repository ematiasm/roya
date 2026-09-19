// Identity kernel: session token minting, hashing, cookie read/write and the
// TTL policy (Slice S1a). The token is 32 bytes of OS entropy carried in
// base64url; only its sha256 digest is ever stored or compared, so a database
// leak cannot mint sessions. The TTL lives in one place (`SessionPolicy`):
// absolute lifetime of `ttl_hours` from login, and a sliding renewal where a
// request after 30 idle minutes extends `expires_at` to `now + TTL`. The
// middleware/extractors that consume this are slice S1b.
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{Duration, NaiveDateTime};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

/// The single cookie name the whole app uses for the session token.
pub const SESSION_COOKIE: &str = "roya_session";

/// Sliding renewal horizon: activity after this idle gap extends the session.
pub const RENEWAL_AFTER_SECS: i64 = 30 * 60;

/// Session lifetime as a cookie `Max-Age` is the absolute TTL in seconds.
const TOKEN_BYTES: usize = 32;

/// Mint a fresh 32-byte token, base64url without padding. The value returned
/// here goes into the cookie exactly once; only `hash_token` of it is stored.
pub fn mint_token() -> AppResult<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| AppError::Internal(format!("session token entropy unavailable: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// sha256 of the token, base64url. The stored form of a session token: same
/// digest in and out of the database, so `resolve` compares equality directly.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Absolute TTL plus the sliding-renewal horizon and the cookie flags, in one
/// struct so no route or test invents its own expiry arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    /// Absolute session lifetime in hours (`ROYA_SESSION_TTL_HOURS`).
    pub ttl_hours: i64,
    /// Adds `Secure` to the cookie (`ROYA_COOKIE_SECURE`); required on HTTPS.
    pub secure: bool,
}

impl SessionPolicy {
    pub fn new(ttl_hours: i64, secure: bool) -> Self {
        Self { ttl_hours, secure }
    }

    pub fn ttl(&self) -> Duration {
        Duration::hours(self.ttl_hours)
    }

    /// `expires_at` for a session minted (or renewed) at `now`.
    pub fn expires_at(&self, now: NaiveDateTime) -> NaiveDateTime {
        now + self.ttl()
    }

    /// Sliding renewal: due only when the session has been idle *strictly past*
    /// the horizon, so activity at exactly 30 minutes does not extend it.
    pub fn renewal_due(&self, last_seen_at: NaiveDateTime, now: NaiveDateTime) -> bool {
        now - last_seen_at > Duration::seconds(RENEWAL_AFTER_SECS)
    }

    /// `Set-Cookie` value for a freshly minted (or renewed) token. Flags are
    /// fixed here, not per-route: HttpOnly, SameSite=Lax, Path=/, Max-Age = the
    /// absolute TTL, Secure when the policy says so.
    pub fn serialize_cookie(&self, token: &str) -> String {
        let max_age = self.ttl_hours * 3600;
        let mut cookie = format!(
            "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}"
        );
        if self.secure {
            cookie.push_str("; Secure");
        }
        cookie
    }

    /// Extract the session token from a `Cookie` request header, ignoring every
    /// other cookie present. Missing cookie or missing token => `None`.
    pub fn parse_cookie(&self, cookie_header: &str) -> Option<String> {
        cookie_header.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            name.eq_ignore_ascii_case(SESSION_COOKIE)
                .then(|| value.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed test timestamp so no test depends on wall-clock time.
    fn base() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 5, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    #[test]
    fn minted_token_is_32_bytes_of_urlsafe_without_padding() {
        let token = mint_token().unwrap();
        assert!(!token.contains('='), "no padding: {token}");
        assert!(!token.contains('+') && !token.contains('/'));
        let decoded = URL_SAFE_NO_PAD.decode(&token).unwrap();
        assert_eq!(decoded.len(), TOKEN_BYTES);
    }

    #[test]
    fn minted_tokens_are_unique() {
        assert_ne!(mint_token().unwrap(), mint_token().unwrap());
    }

    #[test]
    fn hash_token_is_sha256_base64url_and_never_the_raw_token() {
        let token = mint_token().unwrap();
        let hash = hash_token(&token);
        assert_ne!(hash, token, "stored digest must not be the raw token");
        assert!(!hash.contains(&token), "raw token must not appear in the digest");
        // Deterministic: same token, same digest (this is what resolve compares).
        assert_eq!(hash, hash_token(&token));
        let decoded = URL_SAFE_NO_PAD.decode(&hash).unwrap();
        assert_eq!(decoded.len(), 32, "sha256 is 32 bytes");
    }

    #[test]
    fn cookie_roundtrip_survives_other_cookies() {
        let policy = SessionPolicy::new(12, false);
        let token = mint_token().unwrap();
        let header = format!("other=1; {}; another=2", policy.serialize_cookie(&token));
        assert_eq!(policy.parse_cookie(&header).as_deref(), Some(token.as_str()));
    }

    #[test]
    fn cookie_parse_returns_none_without_the_session_cookie() {
        let policy = SessionPolicy::new(12, false);
        assert_eq!(policy.parse_cookie(""), None);
        assert_eq!(policy.parse_cookie("other=1; more=2"), None);
        assert_eq!(policy.parse_cookie("roya_session"), None, "no '=' means no value");
    }

    #[test]
    fn cookie_carries_the_required_flags_and_secure_only_when_asked() {
        let plain = SessionPolicy::new(12, false).serialize_cookie("tok");
        assert!(plain.contains("roya_session=tok"));
        assert!(plain.contains("HttpOnly"));
        assert!(plain.contains("SameSite=Lax"));
        assert!(plain.contains("Path=/"));
        assert!(plain.contains("Max-Age=43200"), "Max-Age is the absolute TTL: {plain}");
        assert!(!plain.contains("Secure"));

        let secure = SessionPolicy::new(12, true).serialize_cookie("tok");
        assert!(secure.ends_with("; Secure"));
    }

    #[test]
    fn expiry_is_absolute_ttl_from_now() {
        let policy = SessionPolicy::new(12, false);
        let now = base();
        assert_eq!(policy.expires_at(now), now + Duration::hours(12));
    }

    #[test]
    fn renewal_is_due_only_strictly_past_30_idle_minutes() {
        let policy = SessionPolicy::new(12, false);
        let last_seen = base();
        // Exactly at the horizon: not due (spec says "older than 30 minutes").
        assert!(!policy.renewal_due(last_seen, last_seen + Duration::seconds(RENEWAL_AFTER_SECS)));
        // One second past it: due.
        assert!(policy.renewal_due(
            last_seen,
            last_seen + Duration::seconds(RENEWAL_AFTER_SECS + 1)
        ));
        // Fresh activity: never due.
        assert!(!policy.renewal_due(last_seen, last_seen + Duration::minutes(29)));
    }
}
