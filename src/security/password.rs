// Identity kernel: argon2id password hashing and verification (Slice S1a).
//
// Production parameters are argon2id at Params::DEFAULT (m=19456 KiB, t=2,
// p=1, the OWASP recommendation); `light()` exists so the test suite can hash
// hundreds of credentials without paying the production cost, and a test pins
// the production parameters so a light-only regression cannot hide. A malformed
// stored hash is a verification failure, not a panic: a corrupt row must never
// crash a login. Nothing in this module logs or returns a password.
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher as _, PasswordVerifier, Version};
use argon2::password_hash::SaltString;

use crate::error::{AppError, AppResult};

/// The hashing/verification surface `IdentityService` depends on, so tests can
/// inject a counting spy instead of a real hasher (honest AC4/AC5 evidence).
pub trait PasswordHashing: Send + Sync {
    /// Hash to an argon2id PHC string with a fresh random salt.
    fn hash(&self, password: &str) -> AppResult<String>;
    /// `false` for a wrong password *and* for a malformed stored hash.
    fn verify(&self, password: &str, stored_hash: &str) -> bool;
}

#[derive(Debug, Clone)]
pub struct PasswordHasher {
    params: Params,
}

impl PasswordHasher {
    /// Production cost: argon2id at `Params::DEFAULT` (OWASP parameters).
    pub fn production() -> Self {
        Self {
            params: Params::DEFAULT,
        }
    }

    /// Test-only cost (~1 MiB / 1 iteration): same API, ~100x cheaper, so the
    /// suite never sleeps on hashing. Never use it outside tests.
    pub fn light() -> Self {
        Self {
            params: Params::new(1024, 1, 1, None)
                // Light parameters are compile-time constants that satisfy
                // argon2's own bounds; a failure would be a crate bug.
                .expect("light argon2 parameters are valid by construction"),
        }
    }
}

impl PasswordHashing for PasswordHasher {
    fn hash(&self, password: &str) -> AppResult<String> {
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, self.params.clone());
        // Salt entropy comes from the crate's own `getrandom` dependency
        // (argon2's rand_core re-export does not expose OsRng without an
        // extra feature); 16 bytes is the argon2-recommended salt length.
        let mut salt = [0u8; 16];
        getrandom::getrandom(&mut salt)
            .map_err(|e| AppError::Internal(format!("password salt entropy unavailable: {e}")))?;
        let salt = SaltString::encode_b64(&salt)
            .map_err(|e| AppError::Internal(format!("password salt encoding failed: {e}")))?;
        argon
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|e| AppError::Internal(format!("password hashing failed: {e}")))
    }

    fn verify(&self, password: &str, stored_hash: &str) -> bool {
        // The PHC string carries its own algorithm, version and cost; the
        // verifier honours them, so a row hashed with different parameters
        // still verifies at its own recorded cost.
        match PasswordHash::new(stored_hash) {
            Ok(parsed) => Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_parameters_meet_the_owasp_floor() {
        // Pinned so `light()` (or a future "fast" tweak) can never silently
        // become the production default: m >= 19456 KiB, t >= 2, p >= 1.
        let p = PasswordHasher::production().params;
        assert!(p.m_cost() >= 19456, "argon2 memory cost below OWASP floor");
        assert!(p.t_cost() >= 2, "argon2 time cost below OWASP floor");
        assert!(p.p_cost() >= 1, "argon2 parallelism below OWASP floor");
    }

    #[test]
    fn hash_produces_argon2id_phc_and_verifies_roundtrip() {
        let hasher = PasswordHasher::light();
        let phc = hasher.hash("correct horse battery staple").unwrap();
        assert!(phc.starts_with("$argon2id$"), "got {phc}");
        assert!(hasher.verify("correct horse battery staple", &phc));
        assert!(!hasher.verify("wrong password", &phc));
    }

    #[test]
    fn each_hash_uses_a_fresh_salt() {
        let hasher = PasswordHasher::light();
        let a = hasher.hash("same password").unwrap();
        let b = hasher.hash("same password").unwrap();
        assert_ne!(a, b, "two hashes of the same password must differ (salt)");
        assert!(hasher.verify("same password", &a));
        assert!(hasher.verify("same password", &b));
    }

    #[test]
    fn malformed_stored_hash_is_a_verification_failure_not_a_panic() {
        let hasher = PasswordHasher::light();
        for bad in [
            "",
            "not a phc string",
            "$argon2id$",
            "$argon2id$v=19$m=0,t=0,p=0$c2FsdA$",
            "$argon2id$v=19$m=19456,t=2,p=1$invalid-base64-salt$invalid-hash",
        ] {
            assert!(!hasher.verify("anything", bad), "got true for {bad:?}");
        }
    }

    #[test]
    fn light_params_are_below_the_owasp_floor_by_design() {
        // Sanity: light() must NOT be usable as production (it would fail the
        // floor the production test pins).
        let p = PasswordHasher::light().params;
        assert!(p.m_cost() < 19456);
    }
}
