// Identity kernel (Slice S1a): transversal security primitives shared by every
// department. This is NOT a department and depends on none: password hashing
// and verification, session token minting/hashing and the TTL policy. The
// middleware and `Require<P>` extractors that sit on top of these are slice
// S1b; the module list below is where they will be declared. Until that wiring
// lands, the whole module is intentionally uncalled from the router.
#![allow(dead_code)]
#![allow(unused_imports)]
pub mod password;
pub mod session;

pub use password::{PasswordHasher, PasswordHashing};
pub use session::{hash_token, mint_token, SessionPolicy, RENEWAL_AFTER_SECS, SESSION_COOKIE};
