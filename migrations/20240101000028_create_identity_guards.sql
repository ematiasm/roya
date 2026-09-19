-- Identity RBAC guards (M5 identity kernel, slice S2). The database holds the
-- line the interface cannot be trusted with: an administrator must always
-- exist, and the protected role is the anchor of that guarantee. These
-- triggers fire for the service layer, for direct SQL and for every future
-- screen; they guard accidental and programmatic writes, and they do not cover
-- an actor deliberately dropping the triggers or altering the schema (the
-- documented scope of invariant 9 in openspec/specs/README.md). Every trigger
-- is named trg_* and refuses with RAISE(ABORT, ...) so the service can map the
-- message to a 409/400 the interface explains in Spanish.
--
-- The guarantees, one trigger each:
--   1. a protected role cannot be deleted (AC13);
--   2. a protected role's code cannot change (AC13: renames are audited, not
--      silently granted to the one role that must always exist);
--   3. a protected role's permission rows cannot be removed (AC13: reducing
--      the administrator's powers would be a lockout in slow motion);
--   4. the last active user holding a protected role cannot be deactivated
--      (AC14: deactivation of the second-to-last succeeds, the last is
--      refused; a second administrator makes the refused operation succeed);
--   5. the last grant of a protected role to an active user cannot be deleted
--      (AC14, same arithmetic on the user_roles table);
--   6. `roles.is_system` cannot be flipped on an existing role (in either
--      direction). The flag is decided at seed time — migration
--      20240101000027 inserts the roles with their final `is_system` value
--      before these triggers exist — because the other guards key off it: a
--      writable flag would let two ordinary statements (flip the flag, then
--      delete the role) erase the protected role and its whole matrix, and
--      with it the guarantee that an administrator always exists;
--   7. a user whose removal would leave no active holder of a protected role
--      cannot be deleted (AC14, the deletion origin of the same arithmetic:
--      the FK cascade into `user_roles` removes the grants while this row
--      still exists, so the user_roles trigger alone would evaluate
--      mid-cascade and let the last administrator's user row go).
--
-- REPLACE-shaped statements (INSERT OR REPLACE / REPLACE INTO) fire delete
-- triggers only with `PRAGMA recursive_triggers = ON`; every pool that can
-- write `roles` or `user_roles` sets that pragma (src/db.rs, shared with the
-- identity test pools). The pragma is per connection: the migration cannot
-- enforce it, so nothing here relies on a REPLACE succeeding.

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
