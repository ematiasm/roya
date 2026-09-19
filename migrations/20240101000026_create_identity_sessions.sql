-- Identity sessions (M5 identity kernel, slice S1a). One row per login: the
-- cookie token is 32 random bytes carried in base64url, and only the sha256
-- digest of that token is stored, so a leaked database never hands out a live
-- session. Validity is decided in SQL (`revoked_at IS NULL AND expires_at >
-- :now` joined to an active user), not only in Rust: revocation, expiry and a
-- deactivated owner are refused by the query itself. Revocation is permanent:
-- the trigger below refuses any UPDATE that clears `revoked_at`, exactly like
-- the walk-in backstops in the customers migration. It guards accidental and
-- programmatic writes; it does not cover someone deliberately dropping the
-- trigger or altering the schema. The permanence guarantee is UPDATE-scoped:
-- a DELETE followed by an INSERT of the same `token_hash` bypasses the
-- trigger, which is enough because a legitimate insert requires the raw
-- cookie token and only its sha256 digest is ever stored.
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash TEXT NOT NULL UNIQUE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    revoked_at TEXT NULL,
    user_agent TEXT NULL
        CONSTRAINT sessions_user_agent_shape CHECK (user_agent IS NULL OR length(user_agent) <= 256)
);

CREATE INDEX IF NOT EXISTS idx_sessions_user_id ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_expires_at ON sessions(expires_at);

CREATE TRIGGER IF NOT EXISTS trg_sessions_revoked_at_immutable
BEFORE UPDATE ON sessions
FOR EACH ROW
WHEN OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS NULL
BEGIN
    SELECT RAISE(ABORT, 'revoked_at cannot be cleared; revocation is permanent');
END;
