-- no-transaction
-- Migration 45: NO METHOD MAY EXIST WITHOUT AN OWNING ACCOUNT. The rule moves
-- into the schema.
--
-- This SUPERSEDES migration 24's documented allowance (`…24:2-3`: "NULL =
-- unassigned, not usable for payments"). From here on `account_id` is NOT NULL:
-- a method is always owned, and "remove a method from an account" means
-- DEACTIVATE it (`is_active = 0`) — it keeps its owner and becomes unusable
-- (`resolve_account_for` refuses an inactive method with an actionable message)
-- instead of vanishing into an ownerless state the product could not see. There
-- is still no method delete: every payment table references `payment_methods`
-- with ON DELETE RESTRICT, so a method with history could not be deleted
-- anyway — deactivation is the only "remove" that exists, and now it is the
-- only one the product offers.
--
-- Three reasons the seed is here rather than in Rust (`ensure_defaults_for_account`
-- stays the runtime path for user-created accounts): a FRESH INSTALL must open
-- collectable — before this migration there is no account at all and every
-- seeded method is unassigned, so nothing can be collected or paid until an
-- operator configures both by hand; and the walk-in precedent (migration 20)
-- already establishes that a document the sale form defaults to must be seeded
-- in the schema, not by an application call that only runs after a login.
--
-- Why `-- no-transaction` on the first line (same technique as migrations 21,
-- 24 and 30, which this file copies): sqlx runs migrations inside a
-- transaction, where PRAGMA foreign_keys is a no-op and a deferred violation
-- from DROP TABLE cannot be healed before COMMIT. Five tables reference
-- `payment_methods(id) ON DELETE RESTRICT` — `sale_payments`,
-- `purchase_payments`, `customer_receipts`, `purchase_return_payments` and
-- `customer_return_payments` — so the parent swap here runs with foreign keys
-- disabled by this migration itself and re-enabled after the swap.
--
-- Why the FIRST statement disables them too, and not just the swap: step (a)
-- below seeds the `Caja` account with a literal `'0'` balance and looks its
-- auditor up by username. On a database migrated from empty that lookup is fine,
-- but the pool opens with foreign keys ON, so every INSERT in this file would be
-- checked eagerly, in statement order, and the seeded account is written before
-- the ledger columns it may need are settled. Writing the seed and the orphan
-- resolution with keys ON makes correctness depend on the statement order of a
-- file that is allowed to be re-ordered by whoever maintains it next. With the
-- PRAGMA off for the whole file, the end of the file is the single place that
-- decides whether the result is consistent, and it decides by measurement — see
-- the hard gate at the bottom. The statements that NEED keys on (the drops and
-- the rename) are exactly the ones that cannot have them on.
--
-- ORDER MATTERS: the seed and the orphan resolution come FIRST, while
-- `account_id` is still NULLable and every payment-history read still sees the
-- unrebuilt table; the rebuild comes last and must find no row left with a
-- NULL account.

PRAGMA foreign_keys = OFF;

-- ---------------------------------------------------------------------------
-- (a) Seed the default account: find-or-create `Caja`.
-- `accounts.name` is UNIQUE, so a blind INSERT aborts on a database that
-- already has one (the walk-in precedent, migration 20:31-34: guarded
-- INSERT ... SELECT ... WHERE NOT EXISTS). The acting user for the audit
-- columns is the `sistema` sentinel migration 30 created — looked up BY NAME,
-- never a hardcoded id. On a fresh install the sentinel exists because
-- migration 30's guard saw the seeded methods (migration 12 populated
-- `payment_methods`); a database where it does not exist cannot exist.
-- ---------------------------------------------------------------------------
INSERT INTO accounts (name, cached_balance, created_by)
SELECT 'Caja', '0',
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE)
WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE name = 'Caja');

-- ---------------------------------------------------------------------------
-- (b) Attach the seeded `Cash` to it — ONLY if Cash has no account yet, and
-- ONLY one of them. Never steal ownership from another account.
--
-- The `id = (SELECT MIN(id) ...)` clause is load-bearing, not decoration: two
-- unassigned `Cash` rows are possible on a legacy database, and without it both
-- would be pointed at Caja and `UNIQUE(account_id, name)` would abort the whole
-- migration. With it, exactly one is adopted and step (c) resolves the other by
-- its history — which is the rule this file already applies to every leftover.
-- ---------------------------------------------------------------------------
UPDATE payment_methods
   SET account_id = (SELECT id FROM accounts WHERE name = 'Caja')
 WHERE name = 'Cash'
   AND account_id IS NULL
   AND id = (SELECT MIN(id) FROM payment_methods
              WHERE name = 'Cash' AND account_id IS NULL);

-- ---------------------------------------------------------------------------
-- (c) Resolve every remaining orphan, by its history.
--
-- One with NO payment history is a seed leftover: DELETE it. History here
-- means a row in ANY of the five payment tables — the three method-choosing
-- ones (`sale_payments`, `purchase_payments`, `customer_receipts`) AND the two
-- refund tables (`customer_return_payments`, `purchase_return_payments`),
-- which replay a parent's pair and are exempt from migration 44's guard but
-- still RESTRICT a method's deletion.
--
-- One WITH history cannot be deleted under ON DELETE RESTRICT: it is attached
-- to the default `Caja` account and DEACTIVATED (`is_active = 0`). The schema
-- holds, history is intact, and a legacy artifact never becomes selectable.
-- A same-named method already owned by Caja IS possible on a database that
-- carried a second unassigned `Cash` (step b adopted exactly one) and it has
-- history: the attach below would then violate `UNIQUE(account_id, name)`. That
-- is deliberate — the migration aborts loudly rather than resolving the clash
-- by renaming a method, which would rewrite what a historical payment refers
-- to. A hand migration is the right response to two same-named methods with
-- history in one business, not a silent merge.
--
-- Deleting must precede the with-history attach: the predicates below are
-- disjoint by history, so the order only avoids one wasted UPDATE.
-- ---------------------------------------------------------------------------
DELETE FROM payment_methods
 WHERE account_id IS NULL
   AND NOT EXISTS (SELECT 1 FROM sale_payments sp WHERE sp.method_id = payment_methods.id)
   AND NOT EXISTS (SELECT 1 FROM purchase_payments pp WHERE pp.method_id = payment_methods.id)
   AND NOT EXISTS (SELECT 1 FROM customer_receipts cr WHERE cr.method_id = payment_methods.id)
   AND NOT EXISTS (SELECT 1 FROM purchase_return_payments prp WHERE prp.method_id = payment_methods.id)
   AND NOT EXISTS (SELECT 1 FROM customer_return_payments crp WHERE crp.method_id = payment_methods.id);

UPDATE payment_methods
   SET account_id = (SELECT id FROM accounts WHERE name = 'Caja'),
       is_active = 0
 WHERE account_id IS NULL;

-- ---------------------------------------------------------------------------
-- (d) Drop the row triggers that live on the CHILD tables and read
-- `payment_methods` from their WHEN clause.
--
-- They must go BEFORE the parent is dropped. SQLite recompiles a trigger's body
-- whenever any ALTER TABLE runs, and after the DROP below the name they resolve
-- (`main.payment_methods`) belongs to nothing; the rename that follows is
-- itself an ALTER TABLE, so the recompile happens while the name is dangling
-- and every later sale payment, purchase payment and customer receipt refuses
-- with "error in trigger ... no such table: main.payment_methods". Dropping
-- them first is the difference between a migration and a broken product.
--
-- The IF EXISTS form tolerates both states this file can find them in, and they
-- are recreated — with migration 44's exact text — in the same statement group
-- that renames the table, so no replay can leave the product unguarded (a
-- second run of this migration is a checksum no-op at the runner anyway).
-- ---------------------------------------------------------------------------
DROP TRIGGER IF EXISTS trg_sale_payments_method_account_insert;
DROP TRIGGER IF EXISTS trg_purchase_payments_method_account_insert;
DROP TRIGGER IF EXISTS trg_customer_receipts_method_account_insert;

-- ---------------------------------------------------------------------------
-- (d) Rebuild `payment_methods` with `account_id INTEGER NOT NULL`, keeping
-- UNIQUE(account_id, name) and every id. Same technique as migrations 24 and
-- 30: CREATE the new shape, INSERT ... SELECT the rows, DROP the old table,
-- RENAME.
--
-- `updated_at` changes shape, and that is the second thing this rebuild
-- decides: it becomes a column DEFAULT instead of a BEFORE INSERT trigger.
--
-- Migration 36 installed `trg_payment_methods_set_updated_at` to replace the
-- `'1970-01-01T00:00:00.000Z'` sentinel with the current time, using
--
--     BEGIN SELECT NEW.updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'); END;
--
-- That statement is a comparison, not an assignment: `SELECT <expr>` discards
-- its result, so the trigger has never stamped anything on any table it was
-- installed on (measured: the sentinel survives the insert verbatim). It was
-- invisible because the only assertion about this column performs an explicit
-- UPDATE first (`src/t1_schema_tests.rs`), and because `create_in_account`
-- reads the value back with RETURNING — which returns the row as the INSERT
-- left it, so a BEFORE INSERT fix would not have helped either.
--
-- A rebuild DROPS the table's triggers, so this file must decide what happens
-- to that one, and re-creating the broken idiom would be worse than deleting
-- it: it would look like a guard. The trigger is dropped with the table and
-- NOT recreated; the default below replaces it. Two consequences, stated
-- rather than discovered:
--
--   * the column is now honest — an INSERT that omits `updated_at` stores the
--     insert time, which is what the trigger was supposed to do;
--   * it is a DEFAULT, not a trigger, so an explicit value is still stored
--     verbatim, which is what the audit test depends on.
--
-- Migrations 36's SIBLINGS on other tables are untouched: the same idiom is
-- broken there too, and fixing five tables is a separate slice with its own
-- evidence, not a side effect of an ownership migration. It is recorded as a
-- follow-up in `odd/tasks/payment-method-single-account.md`.
--
-- The swap DOES need keys off, and the PRAGMA at the top of this file is still
-- in force: `DROP TABLE payment_methods` would otherwise be refused by the five
-- RESTRICT children.
-- ---------------------------------------------------------------------------
CREATE TABLE payment_methods_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (account_id, name)
);

INSERT INTO payment_methods_new (id, name, account_id, is_active, created_by, updated_by, created_at, updated_at)
SELECT id, name, account_id, is_active, created_by, updated_by, created_at, updated_at
FROM payment_methods;

DROP TABLE payment_methods;

ALTER TABLE payment_methods_new RENAME TO payment_methods;

-- Migration 44's three guards, recreated with 44's exact text (the WHEN clause
-- name is resolved at fire time, which is why they must exist again before any
-- payment is written).
CREATE TRIGGER trg_sale_payments_method_account_insert
BEFORE INSERT ON sale_payments
FOR EACH ROW
WHEN NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;

CREATE TRIGGER trg_purchase_payments_method_account_insert
BEFORE INSERT ON purchase_payments
FOR EACH ROW
WHEN NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;

CREATE TRIGGER trg_customer_receipts_method_account_insert
BEFORE INSERT ON customer_receipts
FOR EACH ROW
WHEN NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;

-- The old table's index dies with it; this recreates it under the same name.
CREATE INDEX IF NOT EXISTS idx_payment_methods_account ON payment_methods(account_id);

PRAGMA foreign_keys = ON;

-- The end of this file is the only place that decides whether the swap is
-- sound, and it decides by measurement rather than by trust. `PRAGMA
-- foreign_key_check` on its own prints violations and lets the migration
-- succeed, which would turn a dangling reference into a runtime failure on the
-- first write that touches the edge. The temp table's CHECK turns the count
-- into a refusal: violations abort the migration here, with the offending
-- table still in the schema.
CREATE TEMP TABLE migration_45_fk_guard (
    violations INTEGER NOT NULL CHECK (violations = 0)
);
INSERT INTO migration_45_fk_guard
SELECT COUNT(*) FROM pragma_foreign_key_check();
DROP TABLE migration_45_fk_guard;
