-- no-transaction
-- Audit on the identity tables themselves (M5 Phase B, slice S13, last
-- content slice): `users`, `roles` and `permissions` now record who created
-- them (`created_by`) and, when the row has been edited, who last did
-- (`updated_by`). `role_permissions` gains no columns: a matrix row inherits
-- the actor of the role it belongs to (the plan's Audit section for join
-- rows), and the matrix edit itself stamps the ROLE's `updated_by` — the edit
-- of the role's authorisation — while `user_roles` has carried its own trail
-- since the RBAC slice (`granted_by`/`granted_at`), which this slice finally
-- surfaces on the users screen.

-- Why the three tables take two different routes:
--
-- 1. `users` gets plain ALTER TABLE ADD COLUMN. Its audit columns CANNOT be
--    NOT NULL and that is the honest shape, not a compromise: the sentinel
--    account has no creator and the bootstrap administrator is created by the
--    system, not by an operator — for those rows the honest value of
--    `created_by`/`updated_by` is NULL, meaning "the system", and the
--    interface renders that ("el sistema") instead of inventing a person.
--    SQLite permits `ADD COLUMN ... REFERENCES users(id)` when the column's
--    default is NULL, so the most referenced table in the schema — twelve
--    audited tables' `created_by`, plus `sessions`, `user_roles` and
--    `role_permissions` point at it — is NOT rebuilt at all. The declared
--    action (`ON DELETE RESTRICT`) is the same one every audited table uses:
--    deleting a user that created another user is refused, exactly like
--    deleting one that recorded a finance row.
--
-- 2. `roles` and `permissions` get the controlled rebuild, the same technique
--    as migrations 30–33: the columns cannot be ALTERed into existence
--    (SQLite refuses a NOT NULL column without a constant default, and a
--    `REFERENCES` clause needs the table rebuild anyway). `roles` is the
--    parent of `role_permissions.role_id` and `user_roles.role_id` (CASCADE
--    and RESTRICT), `permissions` is the parent of
--    `role_permissions.permission_id` (CASCADE), and sqlx runs migrations
--    inside a transaction where `PRAGMA foreign_keys` is a no-op and a
--    deferred violation from DROP TABLE cannot be healed before COMMIT — so
--    this migration is marked `-- no-transaction` and disables foreign keys
--    itself for the swap, re-enabling after. With keys off the drops are
--    inert, child rows are untouched, and every id is preserved by the
--    INSERT ... SELECT, so the re-enabled constraints find the same graph
--    that existed before.
--
-- Rebuild discipline around the seven guard triggers (migration 28): the
-- triggers ARE the guarantee that an administrator always exists — the
-- protected-role refusals (delete, code rename, matrix removal, is_system
-- flip) and the last-administrator arithmetic (deactivation, user-row
-- deletion, last-grant removal) reference `roles`, `users` and `user_roles`.
-- All seven are dropped EXPLICITLY before the rebuild and recreated
-- byte-identically at the end (the definitions are copied from migration 28
-- unchanged; `IF NOT EXISTS` keeps a replay idempotent). The runtime proofs
-- that they still bite live in the repositories' and smoke suites' refusal
-- tests, which run every pool through this migration and assert the raw
-- trigger texts: `role_repo`'s AC13/AC14 suite (protected delete, protected
-- code, protected matrix, is_system flip, holder deactivation, user-row
-- deletion, last-grant removal, both REPLACE holes) and the smoke suite's
-- upgrade test below them. Nothing else in the schema references
-- `permissions`, and no session-related or receipt-related trigger
-- (`trg_sessions_revoked_at_immutable`, `trg_customers_walkin_*`,
-- `trg_sale_payments_receipt_*`) is touched.
--
-- Rebuild scope, stated exactly: `roles` is migration 27's definition (no
-- later migration ever touched it) plus the two audit columns;
-- `permissions` is migration 27's definition plus the audit columns (its
-- `updated_by` stays NULL forever — the catalog is never written at runtime;
-- the column exists so the schema is uniform and the drift is visible, not
-- so the interface can invent an editor). `users` keeps migration 25's
-- definition plus the two added columns; its CHECK constraints, its NOCASE
-- unique index and its is_active index are untouched by an ALTER.
--
-- Actor for the pre-existing rows: the sentinel migrations 30, 31, 32 and 33
-- reuse. On every database the chain builds the sentinel is already here —
-- migration 27 seeds the four roles and the 23 permissions, migration 30
-- finds the five seeded payment methods and inserts `sistema` — so the
-- guarded insert below is DEFENSIVE: it fires only if the sentinel is
-- somehow absent when this migration runs, and there is always something to
-- attribute (migration 27's seeds), so on a database built outside the chain
-- whose users table is empty this insert still creates the sentinel. The
-- attribution rule is the honest one the whole change carries (see
-- migration 30's header and openspec/changes/2026-09-19-add-actor-audit/
-- spec.md): rows that predate the audit were not created by any person the
-- system knew — the seeded roles and the permission catalog among them — so
-- they are attributed to the inactive, roleless account whose stored hash is
-- deliberately malformed, never to a person who did not create them. The
-- `users` rows that predate this migration (the sentinel itself, and any
-- account an S1a-era database still holds) get NULL: created by the system,
-- not by an operator.
--
-- Idempotent guards (same discipline as migrations 27, 30, 31, 32 and 33):
-- the sentinel insert is guarded with WHERE NOT EXISTS, so replaying the
-- statement cannot duplicate the row; if an operator already owns a user
-- named `sistema`, their row is the attribution target and no second one is
-- created.

PRAGMA foreign_keys = OFF;

INSERT INTO users (username, display_name, password_hash, is_active)
SELECT 'sistema',
       'Sistema (anterior al registro)',
       -- Malformed on purpose: `PasswordVerifier` answers false for a hash it
       -- cannot parse (tested in security/password.rs), and the account is
       -- inactive on top, so no credential can ever log it in. Byte-identical
       -- to migrations 30's, 31's, 32's and 33's sentinel so the statements
       -- cannot drift apart.
       '$sentinel$no-login-credential$',
       0
WHERE NOT EXISTS (SELECT 1 FROM users WHERE username = 'sistema' COLLATE NOCASE)
  AND ( EXISTS (SELECT 1 FROM roles)
     OR EXISTS (SELECT 1 FROM permissions) );

-- ---------------------------------------------------------------------------
-- users: the nullable, self-referencing audit columns, ALTERed on. No rebuild:
-- SQLite refuses ADD COLUMN only for a NOT NULL column without a constant
-- default and for a REFERENCES clause with a non-NULL default; both columns
-- here are nullable with a NULL default, so the most depended-on table in the
-- schema is left exactly as migration 25 built it. NULL is the honest value
-- for the rows the system created (the sentinel, the bootstrap administrator)
-- and the interface renders it as "el sistema", never as a blank or an id.
-- ---------------------------------------------------------------------------
ALTER TABLE users
    ADD COLUMN created_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT;
ALTER TABLE users
    ADD COLUMN updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT;

-- ---------------------------------------------------------------------------
-- The seven identity guard triggers, dropped before the two rebuilds and
-- recreated byte-identically at the end. The ON-roles triggers would be
-- dropped implicitly by DROP TABLE roles; dropping all seven explicitly makes
-- the recreation the single point where the guarantee text lives in this
-- migration, and keeps a partial replay from leaving a half-set.
-- ---------------------------------------------------------------------------
DROP TRIGGER IF EXISTS trg_roles_protected_no_delete;
DROP TRIGGER IF EXISTS trg_roles_protected_code_immutable;
DROP TRIGGER IF EXISTS trg_role_permissions_protected_no_delete;
DROP TRIGGER IF EXISTS trg_users_last_protected_holder_no_deactivate;
DROP TRIGGER IF EXISTS trg_roles_protected_is_system_immutable;
DROP TRIGGER IF EXISTS trg_users_last_protected_holder_no_delete;
DROP TRIGGER IF EXISTS trg_user_roles_last_protected_grant_no_delete;

-- ---------------------------------------------------------------------------
-- roles: rebuild with the audit columns. Migration 27's definition is the
-- live one — no later migration ever touched this table — so the code shape
-- CHECK, the UNIQUE code, the name/description CHECKs, the is_system CHECK
-- and both timestamp defaults are preserved declaration-for-declaration.
-- ---------------------------------------------------------------------------
CREATE TABLE roles_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code TEXT NOT NULL UNIQUE
        CONSTRAINT roles_code_shape CHECK (
            length(code) BETWEEN 2 AND 64
            AND code GLOB '[a-z]*'
            AND code NOT GLOB '*[^a-z0-9_]*'
        ),
    name TEXT NOT NULL
        CONSTRAINT roles_name_shape CHECK (length(name) BETWEEN 1 AND 128),
    description TEXT NULL
        CONSTRAINT roles_description_shape CHECK (description IS NULL OR length(description) <= 256),
    is_system INTEGER NOT NULL DEFAULT 0
        CONSTRAINT roles_is_system_flag CHECK (is_system IN (0, 1)),
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO roles_new (id, code, name, description, is_system,
                       created_by, updated_by, created_at, updated_at)
SELECT id, code, name, description, is_system,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at, updated_at
FROM roles;

DROP TABLE roles;
ALTER TABLE roles_new RENAME TO roles;

CREATE INDEX IF NOT EXISTS idx_roles_is_system ON roles(is_system);

-- ---------------------------------------------------------------------------
-- permissions: rebuild with the audit columns. Migration 27's definition is
-- the live one; the UNIQUE code is inline and is preserved as declared. The
-- catalog is seeded by migration and never written at runtime, so
-- `updated_by` has no writer — kept NULL forever by design, documented above.
-- ---------------------------------------------------------------------------
CREATE TABLE permissions_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code TEXT NOT NULL UNIQUE,
    module TEXT NOT NULL,
    action TEXT NOT NULL,
    description TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO permissions_new (id, code, module, action, description,
                             created_by, updated_by, created_at)
SELECT id, code, module, action, description,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM permissions;

DROP TABLE permissions;
ALTER TABLE permissions_new RENAME TO permissions;

-- ---------------------------------------------------------------------------
-- The seven guard triggers, recreated byte-identically from migration 28.
-- These are the guarantee that an administrator always exists; the refusal
-- suites prove they still bite against every pool this migration builds.
-- ---------------------------------------------------------------------------

CREATE TRIGGER IF NOT EXISTS trg_roles_protected_no_delete
BEFORE DELETE ON roles
FOR EACH ROW
WHEN OLD.is_system = 1
BEGIN
    SELECT RAISE(ABORT, 'protected role cannot be deleted');
END;

CREATE TRIGGER IF NOT EXISTS trg_roles_protected_code_immutable
BEFORE UPDATE ON roles
FOR EACH ROW
WHEN OLD.is_system = 1 AND NEW.code != OLD.code
BEGIN
    SELECT RAISE(ABORT, 'protected role code cannot change');
END;

CREATE TRIGGER IF NOT EXISTS trg_role_permissions_protected_no_delete
BEFORE DELETE ON role_permissions
FOR EACH ROW
WHEN EXISTS (
    SELECT 1 FROM roles r
    WHERE r.id = OLD.role_id AND r.is_system = 1
)
BEGIN
    SELECT RAISE(ABORT, 'protected role permissions cannot be removed');
END;

CREATE TRIGGER IF NOT EXISTS trg_users_last_protected_holder_no_deactivate
BEFORE UPDATE ON users
FOR EACH ROW
WHEN OLD.is_active = 1 AND NEW.is_active = 0
    AND EXISTS (
        SELECT 1
        FROM user_roles ur
        JOIN roles r ON r.id = ur.role_id
        WHERE ur.user_id = OLD.id AND r.is_system = 1
    )
    AND NOT EXISTS (
        SELECT 1
        FROM user_roles ur2
        JOIN users u2 ON u2.id = ur2.user_id
        JOIN roles r2 ON r2.id = ur2.role_id
        WHERE r2.is_system = 1
          AND u2.is_active = 1
          AND u2.id != OLD.id
    )
BEGIN
    SELECT RAISE(ABORT, 'cannot deactivate the last active user holding a protected role');
END;

CREATE TRIGGER IF NOT EXISTS trg_roles_protected_is_system_immutable
BEFORE UPDATE OF is_system ON roles
FOR EACH ROW
WHEN OLD.is_system != NEW.is_system
BEGIN
    SELECT RAISE(ABORT, 'protected status is decided at seed time and cannot change');
END;

CREATE TRIGGER IF NOT EXISTS trg_users_last_protected_holder_no_delete
BEFORE DELETE ON users
FOR EACH ROW
-- Evaluated before any FK cascade: `user_roles` still holds this user's rows,
-- which is what tells the trigger whether the deleted user holds a protected
-- role and whether an active holder remains after the removal.
WHEN EXISTS (
    SELECT 1
    FROM user_roles ur
    JOIN roles r ON r.id = ur.role_id
    WHERE ur.user_id = OLD.id AND r.is_system = 1
)
    AND NOT EXISTS (
        SELECT 1
        FROM user_roles ur2
        JOIN users u2 ON u2.id = ur2.user_id
        JOIN roles r2 ON r2.id = ur2.role_id
        WHERE r2.is_system = 1
          AND u2.is_active = 1
          AND u2.id != OLD.id
    )
BEGIN
    SELECT RAISE(ABORT, 'cannot delete the last active user holding a protected role');
END;

CREATE TRIGGER IF NOT EXISTS trg_user_roles_last_protected_grant_no_delete
BEFORE DELETE ON user_roles
FOR EACH ROW
WHEN EXISTS (
    SELECT 1 FROM roles r
    WHERE r.id = OLD.role_id AND r.is_system = 1
)
    AND EXISTS (
        SELECT 1 FROM users u
        WHERE u.id = OLD.user_id AND u.is_active = 1
    )
    AND NOT EXISTS (
        SELECT 1
        FROM user_roles ur2
        JOIN users u2 ON u2.id = ur2.user_id
        WHERE ur2.role_id = OLD.role_id
          AND u2.is_active = 1
          AND u2.id != OLD.user_id
    )
BEGIN
    SELECT RAISE(ABORT, 'cannot remove the last grant of a protected role to an active user');
END;

PRAGMA foreign_keys = ON;
