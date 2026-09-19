-- Identity users (M5 identity kernel, slice S1a). The username is the login
-- key: 3-64 chars, lowercase ASCII alnum with dots/underscores/hyphens in the
-- middle (^[a-z0-9]([a-z0-9._-]*[a-z0-9])?$), unique case-insensitively through
-- a COLLATE NOCASE unique index, so `Admin` and `admin` cannot both exist. The
-- CHECK constraints here are the database backstop for the same shape the
-- service validates and reports as Validation. password_hash is an argon2id
-- PHC string and is never returned by any read path: the ordinary User model
-- carries no hash, and UserWithHash exists only for credential verification.
-- Users are deactivated (is_active = 0), never deleted; sessions reference
-- them with CASCADE, while business history will RESTRICT through the audit
-- columns Phase B adds. last_login_at is written through the injected clock,
-- so tests never sleep.
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL
        CONSTRAINT users_username_shape CHECK (
            length(username) BETWEEN 3 AND 64
            AND username GLOB '[a-z0-9]*[a-z0-9]'
            AND username NOT GLOB '*[^a-z0-9._-]*'
        ),
    display_name TEXT NOT NULL
        CONSTRAINT users_display_name_shape CHECK (length(display_name) BETWEEN 1 AND 128),
    password_hash TEXT NOT NULL,
    is_active INTEGER NOT NULL DEFAULT 1
        CONSTRAINT users_is_active_flag CHECK (is_active IN (0, 1)),
    must_change_password INTEGER NOT NULL DEFAULT 0
        CONSTRAINT users_must_change_flag CHECK (must_change_password IN (0, 1)),
    last_login_at TEXT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_users_is_active ON users(is_active);
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_username
    ON users(username COLLATE NOCASE);
