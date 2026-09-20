-- no-transaction
-- Audit on the sales, sale-payment, customer-receipt and customer tables (M5
-- Phase B, slice S11): every row of `sales`, `sale_payments`,
-- `customer_receipts` and `customers` records who created it (`created_by`,
-- NOT NULL) and, when applicable, who last changed it (`updated_by`, NULL).
-- The columns are ordinary integers with a RESTRICT foreign key to `users`; the
-- owning modules write them from the `Principal` each request resolves, so the
-- sales and customers departments never read identity tables (spec invariant
-- 1, AC20). `sale_lines` gains no columns: like the document lines and join
-- rows in the plan's Audit section, a line inherits the actor of its parent
-- document (the sale), so a line is as attributable as the sale it belongs to
-- without duplicating the columns.
--
-- Same technique as migrations 30 and 31 (the finance and inventory audits)
-- and, before them, migration 21 (`add_sales_customer`) and migration 24
-- (`payment_methods_single_account`): the columns cannot be `ALTER TABLE`d into
-- existence (SQLite refuses a NOT NULL column without a constant default, and
-- a `REFERENCES` clause needs the table rebuild anyway), and sqlx runs
-- migrations inside a transaction where PRAGMA foreign_keys is a no-op and a
-- deferred violation from DROP TABLE cannot be healed before COMMIT. So this
-- migration is marked `-- no-transaction` and disables foreign keys itself for
-- the swap, re-enabling after. Dropping a rebuilt parent (customers is the
-- parent of sales.customer_id and customer_receipts.customer_id, both RESTRICT;
-- sales is the parent of sale_lines and sale_payments, both CASCADE;
-- customer_receipts is the parent of sale_payments.receipt_id, RESTRICT) would
-- otherwise fire RESTRICT refusals or CASCADE deletions; with keys off the drop
-- is inert, child rows are untouched, and every id is preserved by the
-- INSERT ... SELECT, so the re-enabled constraints find the same graph that
-- existed before.
--
-- The actor for pre-existing rows: the sentinel migrations 30 and 31 created.
-- THIS MIGRATION REUSES IT — on every database the chain builds the sentinel
-- is already here, because migration 12 seeds the five payment methods and
-- migration 30 therefore always finds finance rows to attribute and inserts
-- the account before this file runs. The guarded insert below is DEFENSIVE:
-- it fires only if the sentinel is somehow absent when this migration runs
-- (a database built outside the chain whose finance and inventory tables were
-- emptied, for example) and only when there is something to attribute — a
-- database with none of the four tables populated gains no user at all. The
-- attribution rule is the same honest one as S9 and S10 (see migration 30's
-- header and openspec/changes/2026-09-19-add-actor-audit/spec.md): rows that
-- predate the audit were not created by any person the system knew — the
-- seeded walk-in customer among them — so they are attributed to the inactive,
-- roleless account whose stored hash is deliberately malformed, never to a
-- person who did not create them.
--
-- Rebuild discipline: the live definition of `sales` is migration 21's (which
-- rebuilt it to enforce the mandatory `customer_id`), not migration 08's
-- original; `sale_payments` carries the `method_id` migration 12 added, the
-- payment/transaction links migration 19 added and the receipt link migration
-- 23 added; `customers` keeps the CHECKs, the partial unique walk-in index and
-- the three walk-in protection triggers migration 20 created, and its seeded
-- walk-in row is NOT re-seeded here (it already exists and keeps its id). Every
-- declaration below is copied from the live state the chain produces, so no
-- constraint, index, default or trigger is silently weaker afterwards.
--
-- Idempotent guards (same discipline as migrations 27, 30 and 31): the
-- sentinel insert is guarded with WHERE NOT EXISTS, so replaying the statement
-- cannot duplicate the row; if an operator already owns a user named
-- `sistema`, their row is the attribution target and no second one is created.

PRAGMA foreign_keys = OFF;

-- The sale_payments triggers of migration 23 reference `sales` and
-- `customer_receipts` by name. SQLite validates every trigger definition when
-- an ALTER TABLE RENAME runs, so leaving them in place would make the
-- `sales_new -> sales` rename below fail with "no such table" while the old
-- bodies still point at the dropped table. They are dropped here and recreated
-- declaration-for-declaration after the sale_payments rebuild, so the
-- invariant they enforce keeps its database backstop.
DROP TRIGGER IF EXISTS trg_sale_payments_receipt_customer_insert;
DROP TRIGGER IF EXISTS trg_sale_payments_receipt_customer_update;

INSERT INTO users (username, display_name, password_hash, is_active)
SELECT 'sistema',
       'Sistema (anterior al registro)',
       -- Malformed on purpose: `PasswordVerifier` answers false for a hash it
       -- cannot parse (tested in security/password.rs), and the account is
       -- inactive on top, so no credential can ever log it in. Byte-identical
       -- to migrations 30's and 31's sentinel so the statements cannot drift
       -- apart.
       '$sentinel$no-login-credential$',
       0
WHERE NOT EXISTS (SELECT 1 FROM users WHERE username = 'sistema' COLLATE NOCASE)
  AND ( EXISTS (SELECT 1 FROM sales)
     OR EXISTS (SELECT 1 FROM sale_payments)
     OR EXISTS (SELECT 1 FROM customer_receipts)
     OR EXISTS (SELECT 1 FROM customers) );

-- ---------------------------------------------------------------------------
-- sales: rebuild with the audit columns. Migration 21's rebuild (the mandatory
-- customer) is the live definition this one preserves; the draft/cancelled
-- NULL sale_number stays legal and the UNIQUE receipt_no stays unique.
-- ---------------------------------------------------------------------------
CREATE TABLE sales_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sale_number TEXT NULL UNIQUE,
    status TEXT NOT NULL CHECK (status IN ('Draft', 'Confirmed', 'Cancelled')),
    payment_type TEXT NOT NULL CHECK (payment_type IN ('Cash', 'Credit')),
    customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE RESTRICT,
    customer_name TEXT NOT NULL DEFAULT '',
    sale_date TEXT NOT NULL,
    due_date TEXT NULL,
    receipt_no TEXT NULL UNIQUE,
    notes TEXT NOT NULL DEFAULT '',
    cancel_reason TEXT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    confirmed_at TEXT NULL,
    cancelled_at TEXT NULL
);

INSERT INTO sales_new (id, sale_number, status, payment_type, customer_id,
                       customer_name, sale_date, due_date, receipt_no, notes,
                       cancel_reason, created_by, updated_by,
                       created_at, updated_at, confirmed_at, cancelled_at)
SELECT id, sale_number, status, payment_type, customer_id,
       customer_name, sale_date, due_date, receipt_no, notes,
       cancel_reason,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at, updated_at, confirmed_at, cancelled_at
FROM sales;

DROP TABLE sales;
ALTER TABLE sales_new RENAME TO sales;

CREATE INDEX IF NOT EXISTS idx_sales_status ON sales(status);
CREATE INDEX IF NOT EXISTS idx_sales_sale_date ON sales(sale_date);
CREATE INDEX IF NOT EXISTS idx_sales_customer_id ON sales(customer_id);

-- ---------------------------------------------------------------------------
-- sale_payments: rebuild with the audit columns. The live definition carries
-- method_id (migration 12, RESTRICT to the seeded methods, the DEFAULT 1 kept
-- exactly as the ALTER TABLE left it), the transaction/refund links (migration
-- 19, both RESTRICT) and the receipt link (migration 23, RESTRICT). The
-- receipt-customer triggers of migration 23 die with the dropped table and are
-- recreated declaration-for-declaration below, so the data invariant they
-- enforce (a payment can only be grouped under a receipt of its own customer)
-- keeps its database backstop.
-- ---------------------------------------------------------------------------
CREATE TABLE sale_payments_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sale_id INTEGER NOT NULL REFERENCES sales(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    method_id INTEGER NOT NULL DEFAULT 1 REFERENCES payment_methods(id) ON DELETE RESTRICT,
    amount TEXT NOT NULL,
    date TEXT NOT NULL,
    transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT,
    refund_transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT,
    receipt_id INTEGER NULL REFERENCES customer_receipts(id) ON DELETE RESTRICT,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (sale_id) REFERENCES sales(id) ON DELETE CASCADE,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE RESTRICT
);

INSERT INTO sale_payments_new (id, sale_id, account_id, method_id, amount, date,
                               transaction_id, refund_transaction_id, receipt_id,
                               created_by, updated_by, created_at)
SELECT id, sale_id, account_id, method_id, amount, date,
       transaction_id, refund_transaction_id, receipt_id,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM sale_payments;

DROP TABLE sale_payments;
ALTER TABLE sale_payments_new RENAME TO sale_payments;

CREATE INDEX IF NOT EXISTS idx_sale_payments_sale_id ON sale_payments(sale_id);
CREATE INDEX IF NOT EXISTS idx_sale_payments_account_id ON sale_payments(account_id);
CREATE INDEX IF NOT EXISTS idx_sale_payments_method_id ON sale_payments(method_id);
CREATE INDEX IF NOT EXISTS idx_sale_payments_transaction_id ON sale_payments(transaction_id);
CREATE INDEX IF NOT EXISTS idx_sale_payments_refund_transaction_id
    ON sale_payments(refund_transaction_id);
CREATE INDEX IF NOT EXISTS idx_sale_payments_receipt_id ON sale_payments(receipt_id);

-- The payment-grouping invariant of migration 23, recreated verbatim: a
-- payment may only be grouped under a receipt of its own customer. These are
-- data invariants, so they fire for direct SQL too, exactly as before.

-- ---------------------------------------------------------------------------
-- customer_receipts: rebuild with the audit columns. The three RESTRICT
-- references (customer, account, method) and both indexes are preserved, so
-- the receipt document keeps every reference it was written with.
-- ---------------------------------------------------------------------------
CREATE TABLE customer_receipts_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE RESTRICT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    method_id INTEGER NOT NULL REFERENCES payment_methods(id) ON DELETE RESTRICT,
    date TEXT NOT NULL,
    notes TEXT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO customer_receipts_new (id, customer_id, account_id, method_id, date, notes,
                                   created_by, updated_by, created_at)
SELECT id, customer_id, account_id, method_id, date, notes,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM customer_receipts;

DROP TABLE customer_receipts;
ALTER TABLE customer_receipts_new RENAME TO customer_receipts;

CREATE INDEX IF NOT EXISTS idx_customer_receipts_customer_id
    ON customer_receipts(customer_id);
CREATE INDEX IF NOT EXISTS idx_customer_receipts_date
    ON customer_receipts(date);

-- ---------------------------------------------------------------------------
-- customers: rebuild with the audit columns. The walk-in rules migration 20
-- built are preserved declaration-for-declaration: the two CHECKs, the partial
-- unique index ("a second walk-in cannot be created") and the three triggers
-- ("the walk-in cannot be deactivated, deleted or demoted"), which die with the
-- dropped table and are recreated below. The seeded walk-in row itself is NOT
-- re-seeded — it already exists in the rebuilt table, attributed to the
-- sentinel with its id preserved.
-- ---------------------------------------------------------------------------
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
    payment_days INTEGER NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO customers_new (id, name, phone, address, tax_id, notes, is_walkin, is_active,
                           credit_limit, payment_days, created_by, updated_by,
                           created_at, updated_at)
SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
       credit_limit, payment_days,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at, updated_at
FROM customers;

DROP TABLE customers;
ALTER TABLE customers_new RENAME TO customers;

CREATE INDEX IF NOT EXISTS idx_customers_is_active ON customers(is_active);
CREATE UNIQUE INDEX IF NOT EXISTS idx_customers_one_walkin
    ON customers(is_walkin) WHERE is_walkin = 1;

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

CREATE TRIGGER IF NOT EXISTS trg_customers_walkin_no_demote
BEFORE UPDATE ON customers
FOR EACH ROW
WHEN OLD.is_walkin = 1 AND NEW.is_walkin <> 1
BEGIN
    SELECT RAISE(ABORT, 'walk-in customer cannot be demoted to a regular customer');
END;

-- ---------------------------------------------------------------------------
-- sale_payments triggers (from migration 23), recreated LAST: they reference
-- `sales` and `customer_receipts` by name, and every ALTER TABLE RENAME above
-- validates the whole schema, so they cannot exist until both rebuilt tables
-- carry their final names. The invariant they enforce — a payment may only be
-- grouped under a receipt of its own customer — keeps its database backstop,
-- firing for direct SQL too, exactly as migration 23 wrote it.
-- ---------------------------------------------------------------------------
CREATE TRIGGER IF NOT EXISTS trg_sale_payments_receipt_customer_insert
BEFORE INSERT ON sale_payments
FOR EACH ROW
WHEN NEW.receipt_id IS NOT NULL
 AND (SELECT customer_id FROM customer_receipts WHERE id = NEW.receipt_id)
  <> (SELECT customer_id FROM sales WHERE id = NEW.sale_id)
BEGIN
    SELECT RAISE(ABORT, 'sale payment cannot be grouped under another customer''s receipt');
END;

CREATE TRIGGER IF NOT EXISTS trg_sale_payments_receipt_customer_update
BEFORE UPDATE ON sale_payments
FOR EACH ROW
WHEN NEW.receipt_id IS NOT NULL
 AND (SELECT customer_id FROM customer_receipts WHERE id = NEW.receipt_id)
  <> (SELECT customer_id FROM sales WHERE id = NEW.sale_id)
BEGIN
    SELECT RAISE(ABORT, 'sale payment cannot be grouped under another customer''s receipt');
END;

PRAGMA foreign_keys = ON;
