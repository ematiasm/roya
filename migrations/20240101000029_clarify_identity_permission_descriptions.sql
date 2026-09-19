-- Clarify the identity permission descriptions (S3 correction round). The
-- seeded text of `identity.users.manage` promised "assign roles", but role
-- assignment is gated `identity.roles.manage` (spec, "Role assignment": the
-- tier that decides who administers the instance); a principal holding only
-- `identity.users.manage` is refused every role-set change. The description
-- is persisted data — it is what the S4 permission matrix renders and what an
-- operator reads before granting — so code and database must say the same
-- thing the gate enforces. The same audit found `identity.roles.manage`
-- underdescribed: its gate also covers changing other accounts' role sets
-- (`/web/users/roles`), the powers the spec's "Cross-account honesty" writes
-- down.
--
-- This migration updates the seeded rows in place, so a database created
-- before this change and a fresh one end identical; the code catalog
-- (`src/security/authz.rs`) carries the same texts and the extended
-- catalog-drift test compares both sides. Migration 27 stays untouched: it is
-- already merged and applied elsewhere.
UPDATE permissions
SET description = 'Crear usuarios y restablecer contraseñas de cuentas sin roles protegidos'
WHERE code = 'identity.users.manage';

UPDATE permissions
SET description = 'Crear roles, editar la matriz de permisos y cambiar los roles de otras cuentas'
WHERE code = 'identity.roles.manage';
