-- no-transaction
-- T1 schema baseline for business configuration, taxes, party due terms, and
-- missing technical audit timestamps. No business configuration or
-- administrator is seeded here; the first-run setup owns those writes.

PRAGMA foreign_keys = OFF;

CREATE TABLE business_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    business_name TEXT NOT NULL,
    default_locale_code TEXT NOT NULL,
    currency_code TEXT NOT NULL,
    timezone TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE business_locales (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    locale_code TEXT NOT NULL UNIQUE,
    language_code TEXT NOT NULL,
    display_name TEXT NOT NULL,
    is_enabled INTEGER NOT NULL DEFAULT 1 CHECK (is_enabled IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE taxes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    rate TEXT NOT NULL,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE product_taxes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    tax_id INTEGER NOT NULL REFERENCES taxes(id) ON DELETE RESTRICT,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (product_id, tax_id)
);

CREATE INDEX idx_product_taxes_product_id ON product_taxes(product_id);
CREATE INDEX idx_product_taxes_tax_id ON product_taxes(tax_id);

-- Technical mutation timestamps for rows that already carry `updated_by`.
-- SQLite permits only a constant default on ADD COLUMN, so the migration uses a
-- sentinel, backfills existing rows, and installs BEFORE INSERT triggers that
-- replace that sentinel with the current timestamp for every later insert.
ALTER TABLE transactions ADD COLUMN updated_at TEXT NOT NULL
    DEFAULT '1970-01-01T00:00:00.000Z';
ALTER TABLE payment_methods ADD COLUMN updated_at TEXT NOT NULL
    DEFAULT '1970-01-01T00:00:00.000Z';
ALTER TABLE categories ADD COLUMN updated_at TEXT NOT NULL
    DEFAULT '1970-01-01T00:00:00.000Z';
ALTER TABLE product_supplier_costs ADD COLUMN updated_at TEXT NOT NULL
    DEFAULT '1970-01-01T00:00:00.000Z';
ALTER TABLE sale_payments ADD COLUMN updated_at TEXT NOT NULL
    DEFAULT '1970-01-01T00:00:00.000Z';
ALTER TABLE purchase_payments ADD COLUMN updated_at TEXT NOT NULL
    DEFAULT '1970-01-01T00:00:00.000Z';

UPDATE transactions SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
UPDATE payment_methods SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
UPDATE categories SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
UPDATE product_supplier_costs SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
UPDATE sale_payments SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
UPDATE purchase_payments SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');

CREATE TRIGGER trg_transactions_set_updated_at
BEFORE INSERT ON transactions
FOR EACH ROW
WHEN NEW.updated_at = '1970-01-01T00:00:00.000Z'
BEGIN
    SELECT NEW.updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
END;

CREATE TRIGGER trg_payment_methods_set_updated_at
BEFORE INSERT ON payment_methods
FOR EACH ROW
WHEN NEW.updated_at = '1970-01-01T00:00:00.000Z'
BEGIN
    SELECT NEW.updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
END;

CREATE TRIGGER trg_categories_set_updated_at
BEFORE INSERT ON categories
FOR EACH ROW
WHEN NEW.updated_at = '1970-01-01T00:00:00.000Z'
BEGIN
    SELECT NEW.updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
END;

CREATE TRIGGER trg_product_supplier_costs_set_updated_at
BEFORE INSERT ON product_supplier_costs
FOR EACH ROW
WHEN NEW.updated_at = '1970-01-01T00:00:00.000Z'
BEGIN
    SELECT NEW.updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
END;

CREATE TRIGGER trg_sale_payments_set_updated_at
BEFORE INSERT ON sale_payments
FOR EACH ROW
WHEN NEW.updated_at = '1970-01-01T00:00:00.000Z'
BEGIN
    SELECT NEW.updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
END;

CREATE TRIGGER trg_purchase_payments_set_updated_at
BEFORE INSERT ON purchase_payments
FOR EACH ROW
WHEN NEW.updated_at = '1970-01-01T00:00:00.000Z'
BEGIN
    SELECT NEW.updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
END;

-- These columns store NaiveDate business dates, not technical timestamps.
ALTER TABLE product_supplier_costs
    RENAME COLUMN current_cost_updated_at TO current_cost_date;
ALTER TABLE product_supplier_costs
    RENAME COLUMN previous_cost_updated_at TO previous_cost_date;

-- Suppliers share the customer term semantics: NULL has no default, zero is
-- due immediately, and every explicit term is non-negative.
ALTER TABLE suppliers ADD COLUMN due_days INTEGER NULL
    CHECK (due_days IS NULL OR due_days >= 0);

-- Rebuild customers to rename the legacy term and enforce the same non-negative
-- domain rule. SQLite cannot add a CHECK to the renamed column in place. The
-- migration preserves every id, the seeded walk-in, indexes, and all three
-- walk-in protection triggers.
CREATE TABLE customers_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    phone TEXT NULL,
    address TEXT NULL,
    tax_id TEXT NULL,
    notes TEXT NULL,
    is_walkin INTEGER NOT NULL DEFAULT 0 CHECK (is_walkin IN (0, 1)),
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    credit_limit TEXT NULL,
    due_days INTEGER NULL CHECK (due_days IS NULL OR due_days >= 0),
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO customers_new (
    id, name, phone, address, tax_id, notes, is_walkin, is_active,
    credit_limit, due_days, created_by, updated_by, created_at, updated_at
)
SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
       credit_limit, payment_days, created_by, updated_by, created_at, updated_at
FROM customers;

DROP TABLE customers;
ALTER TABLE customers_new RENAME TO customers;

CREATE INDEX idx_customers_is_active ON customers(is_active);
CREATE UNIQUE INDEX idx_customers_one_walkin
    ON customers(is_walkin) WHERE is_walkin = 1;

CREATE TRIGGER trg_customers_walkin_no_deactivate
BEFORE UPDATE ON customers
FOR EACH ROW
WHEN OLD.is_walkin = 1 AND NEW.is_active = 0
BEGIN
    SELECT RAISE(ABORT, 'walk-in customer cannot be deactivated');
END;

CREATE TRIGGER trg_customers_walkin_no_delete
BEFORE DELETE ON customers
FOR EACH ROW
WHEN OLD.is_walkin = 1
BEGIN
    SELECT RAISE(ABORT, 'walk-in customer cannot be deleted');
END;

CREATE TRIGGER trg_customers_walkin_no_demote
BEFORE UPDATE ON customers
FOR EACH ROW
WHEN OLD.is_walkin = 1 AND NEW.is_walkin <> 1
BEGIN
    SELECT RAISE(ABORT, 'walk-in customer cannot be demoted to a regular customer');
END;

PRAGMA foreign_keys = ON;
