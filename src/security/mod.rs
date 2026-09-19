// Identity kernel (Slice S1a/S1b): transversal security primitives shared by
// every department. This is NOT a department: password hashing and
// verification, session token minting/hashing, the TTL policy and the
// deny-by-default gate that consumes `AppState` (which itself depends on the
// kernel, not the other way around). `Require<P>` extractors arrive with S2.
pub mod authz;
pub mod guard;
pub mod password;
pub mod session;

// Test-only support (S1b part 1): the fixed test session every HTTP test
// authenticates with. Compiled out of production builds.
#[cfg(test)]
pub mod test_support;

pub use guard::auth_middleware;
pub use password::PasswordHasher;
pub use session::SessionPolicy;
