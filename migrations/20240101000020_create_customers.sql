-- Customers (M4 clientes, slice K1). A real customer entity, standalone for now:
-- sales.customer_id arrives in a later migration, so nothing here references or
-- reads the sales tables yet. name is NOT unique on purpose: two people can share
-- a name, so the service reports existing exact-name matches as a warning and
-- never blocks. phone/address/tax_id/notes are optional free text; name/phone/
-- address/tax_id/notes lengths and trimming are validated by the service.
-- credit_limit is Decimal-as-TEXT like the rest of the project, NULL means no
-- limit. payment_days is the default credit term, NULL means no default.
-- The single walk-in row is seeded with a guarded INSERT ... WHERE NOT EXISTS so
-- re-running the statement cannot duplicate it; the partial unique index is the
-- database backstop for "a second walk-in cannot be created".
CREATE TABLE IF NOT EXISTS customers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    phone TEXT NULL,
    address TEXT NULL,
    tax_id TEXT NULL,
    notes TEXT NULL,
    is_walkin INTEGER NOT NULL DEFAULT 0 CHECK (is_walkin IN (0, 1)),
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    credit_limit TEXT NULL,
    payment_days INTEGER NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_customers_is_active ON customers(is_active);
CREATE UNIQUE INDEX IF NOT EXISTS idx_customers_one_walkin
    ON customers(is_walkin) WHERE is_walkin = 1;

-- Seed exactly one walk-in customer for cash sales. Guarded so a re-run (or a
-- manual replay of this statement) leaves the existing row untouched.
INSERT INTO customers (name, is_walkin, credit_limit, payment_days)
SELECT 'Consumidor final', 1, NULL, NULL
WHERE NOT EXISTS (SELECT 1 FROM customers WHERE is_walkin = 1);

-- Database backstop for the seeded walk-in: a deactivated or deleted walk-in
-- would silently break cash selling (the sale form defaults to it), so the
-- invariant lives in the schema, not only in the service. These triggers fire
-- for direct SQL too; CustomerService keeps its own clearer Validation errors.
-- Renaming the walk-in or editing its contact fields stays allowed: only
-- `is_active = 0` and DELETE are aborted.
CREATE TRIGGER IF NOT EXISTS trg_customers_walkin_no_deactivate
BEFORE UPDATE ON customers
FOR EACH ROW
WHEN OLD.is_walkin = 1 AND NEW.is_active = 0
BEGIN
    SELECT RAISE(ABORT, 'walk-in customer cannot be deactivated');
END;

CREATE TRIGGER IF NOT EXISTS trg_customers_walkin_no_delete
BEFORE DELETE ON customers
FOR EACH ROW
WHEN OLD.is_walkin = 1
BEGIN
    SELECT RAISE(ABORT, 'walk-in customer cannot be deleted');
END;

-- The walk-in is permanent: demoting it (`is_walkin` leaving 1) would defeat
-- both triggers above, which key on `OLD.is_walkin = 1`, and leave the
-- invariant only half enforced. The condition rejects any new value other than
-- 1, not just 0, so `PRAGMA ignore_check_constraints` cannot smuggle 2 through.
-- Together the three triggers express: exactly one walk-in exists and it is
-- permanent.
--
-- Scope of the guarantee: these triggers guard against accidental and
-- programmatic writes (including REPLACE conflict resolution, which is why the
-- app enables `PRAGMA recursive_triggers`). They do NOT protect against
-- someone deliberately altering the schema: `DROP TRIGGER` removes them and a
-- schema owner can always do that.
CREATE TRIGGER IF NOT EXISTS trg_customers_walkin_no_demote
BEFORE UPDATE ON customers
FOR EACH ROW
WHEN OLD.is_walkin = 1 AND NEW.is_walkin <> 1
BEGIN
    SELECT RAISE(ABORT, 'walk-in customer cannot be demoted to a regular customer');
END;
