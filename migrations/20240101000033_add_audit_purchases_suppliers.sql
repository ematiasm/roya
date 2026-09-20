-- no-transaction
-- Audit on the purchase, purchase-payment, supplier and cost tables (M5
-- Phase B, slice S12): every row of `purchases`, `purchase_payments`,
-- `suppliers` and `product_supplier_costs` records who created it (`created_by`,
-- NOT NULL) and, when applicable, who last changed it (`updated_by`, NULL).
-- The columns are ordinary integers with a RESTRICT foreign key to `users`; the
-- owning modules write them from the `Principal` each request resolves, so the
-- purchases and suppliers departments never read identity tables (spec
-- invariant 1, AC20). `purchase_lines` gains no columns: like the document
-- lines and join rows in the plan's Audit section, a line inherits the actor of
-- its parent document (the purchase), so a line is as attributable as the
-- purchase it belongs to without duplicating the columns.
--
-- Same technique as migrations 30, 31 and 32 (the finance, inventory and
-- sales/customers audits) and, before them, migration 21
-- (`add_sales_customer`) and migration 24 (`payment_methods_single_account`):
-- the columns cannot be `ALTER TABLE`d into existence (SQLite refuses a NOT
-- NULL column without a constant default, and a `REFERENCES` clause needs the
-- table rebuild anyway), and sqlx runs migrations inside a transaction where
-- PRAGMA foreign_keys is a no-op and a deferred violation from DROP TABLE
-- cannot be healed before COMMIT. So this migration is marked
-- `-- no-transaction` and disables foreign keys itself for the swap,
-- re-enabling after. Dropping a rebuilt parent (suppliers is the parent of
-- purchases.supplier_id and product_supplier_costs.supplier_id, both RESTRICT;
-- purchases is the parent of purchase_lines and purchase_payments, both
-- CASCADE; products is the parent of product_supplier_costs.product_id,
-- RESTRICT; accounts and payment_methods parent purchase_payments' links,
-- RESTRICT) would otherwise fire RESTRICT refusals or CASCADE deletions; with
-- keys off the drop is inert, child rows are untouched, and every id is
-- preserved by the INSERT ... SELECT, so the re-enabled constraints find the
-- same graph that existed before.
--
-- The actor for pre-existing rows: the sentinel migrations 30, 31 and 32
-- reuse. THIS MIGRATION REUSES IT — on every database the chain builds the
-- sentinel is already here, because migration 12 seeds the five payment
-- methods and migration 30 therefore always finds finance rows to attribute
-- and inserts the account before this file runs. The guarded insert below is
-- DEFENSIVE: it fires only if the sentinel is somehow absent when this
-- migration runs (a database built outside the chain whose finance, inventory
-- and sales tables were emptied, for example) and only when there is something
-- to attribute — a database with none of the four tables populated gains no
-- user at all. The attribution rule is the same honest one as S9, S10 and S11
-- (see migration 30's header and
-- openspec/changes/2026-09-19-add-actor-audit/spec.md): rows that predate the
-- audit were not created by any person the system knew — the seeded payment
-- methods' rows among them — so they are attributed to the inactive, roleless
-- account whose stored hash is deliberately malformed, never to a person who
-- did not create them.
--
-- Rebuild discipline: the live definition of each table is its original
-- migration plus every later one that touched it. `suppliers` is exactly
-- migration 13's (nothing else ever touched it); `product_supplier_costs` is
-- exactly migration 14's (same); `purchases` is exactly migration 15's (same);
-- `purchase_payments` is migration 17's plus the transaction/refund links
-- migration 19 ALTERed on. No trigger references any of the four tables (the
-- schema's triggers cover identity_sessions, the walk-in customer and the
-- sale_payments receipt grouping), so unlike migration 32 nothing has to be
-- dropped and recreated around the renames. Every declaration below is copied
-- from the live state the chain produces, so no constraint, index, default or
-- trigger is silently weaker afterwards.
--
-- Idempotent guards (same discipline as migrations 27, 30, 31 and 32): the
-- sentinel insert is guarded with WHERE NOT EXISTS, so replaying the statement
-- cannot duplicate the row; if an operator already owns a user named
-- `sistema`, their row is the attribution target and no second one is created.

PRAGMA foreign_keys = OFF;

INSERT INTO users (username, display_name, password_hash, is_active)
SELECT 'sistema',
       'Sistema (anterior al registro)',
       -- Malformed on purpose: `PasswordVerifier` answers false for a hash it
       -- cannot parse (tested in security/password.rs), and the account is
       -- inactive on top, so no credential can ever log it in. Byte-identical
       -- to migrations 30's, 31's and 32's sentinel so the statements cannot
       -- drift apart.
       '$sentinel$no-login-credential$',
       0
WHERE NOT EXISTS (SELECT 1 FROM users WHERE username = 'sistema' COLLATE NOCASE)
  AND ( EXISTS (SELECT 1 FROM suppliers)
     OR EXISTS (SELECT 1 FROM product_supplier_costs)
     OR EXISTS (SELECT 1 FROM purchases)
     OR EXISTS (SELECT 1 FROM purchase_payments) );

-- ---------------------------------------------------------------------------
-- suppliers: rebuild with the audit columns. Migration 13's definition is the
-- live one — no later migration ever touched this table — so the UNIQUE name,
-- the is_active CHECK and both timestamp defaults are preserved
-- declaration-for-declaration.
-- ---------------------------------------------------------------------------
CREATE TABLE suppliers_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    phone TEXT NULL,
    notes TEXT NULL,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO suppliers_new (id, name, phone, notes, is_active,
                           created_by, updated_by, created_at, updated_at)
SELECT id, name, phone, notes, is_active,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at, updated_at
FROM suppliers;

DROP TABLE suppliers;
ALTER TABLE suppliers_new RENAME TO suppliers;

CREATE INDEX IF NOT EXISTS idx_suppliers_is_active ON suppliers(is_active);

-- ---------------------------------------------------------------------------
-- product_supplier_costs: rebuild with the audit columns. Migration 14's
-- definition is the live one — no later migration ever touched this table —
-- so the RESTRICT product/supplier references (each written inline AND as an
-- explicit FOREIGN KEY clause, exactly as the original did), the
-- UNIQUE(product_id, supplier_id) pair, the is_preferred CHECK and the partial
-- unique "one preferred per product" index are preserved as declared.
-- ---------------------------------------------------------------------------
CREATE TABLE product_supplier_costs_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    supplier_id INTEGER NOT NULL REFERENCES suppliers(id) ON DELETE RESTRICT,
    current_cost TEXT NOT NULL,
    current_cost_updated_at TEXT NOT NULL,
    previous_cost TEXT NULL,
    previous_cost_updated_at TEXT NULL,
    is_preferred INTEGER NOT NULL DEFAULT 0 CHECK (is_preferred IN (0, 1)),
    supplier_sku TEXT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (product_id, supplier_id),
    FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT,
    FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE RESTRICT
);

INSERT INTO product_supplier_costs_new (id, product_id, supplier_id, current_cost,
                                        current_cost_updated_at, previous_cost,
                                        previous_cost_updated_at, is_preferred,
                                        supplier_sku, created_by, updated_by, created_at)
SELECT id, product_id, supplier_id, current_cost,
       current_cost_updated_at, previous_cost,
       previous_cost_updated_at, is_preferred,
       supplier_sku,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM product_supplier_costs;

DROP TABLE product_supplier_costs;
ALTER TABLE product_supplier_costs_new RENAME TO product_supplier_costs;

CREATE INDEX IF NOT EXISTS idx_product_supplier_costs_product_id
    ON product_supplier_costs(product_id);
CREATE INDEX IF NOT EXISTS idx_product_supplier_costs_supplier_id
    ON product_supplier_costs(supplier_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_product_supplier_costs_one_preferred
    ON product_supplier_costs(product_id) WHERE is_preferred = 1;

-- ---------------------------------------------------------------------------
-- purchases: rebuild with the audit columns. Migration 15's definition is the
-- live one — no later migration ever touched this table — so the NULL-only-in-
-- Draft UNIQUE purchase_number, the status/payment_type CHECKs and the
-- RESTRICT supplier reference (inline AND explicit, as the original wrote it)
-- are preserved as declared. purchase_lines keeps pointing at `purchases` by
-- name: the rebuild preserves every id, so the CASCADE graph is unchanged.
-- ---------------------------------------------------------------------------
CREATE TABLE purchases_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    purchase_number TEXT NULL UNIQUE,
    supplier_id INTEGER NOT NULL REFERENCES suppliers(id) ON DELETE RESTRICT,
    status TEXT NOT NULL CHECK (status IN ('Draft', 'Confirmed', 'Cancelled')),
    payment_type TEXT NOT NULL CHECK (payment_type IN ('Cash', 'Credit')),
    purchase_date TEXT NOT NULL,
    due_date TEXT NULL,
    supplier_invoice_no TEXT NULL,
    notes TEXT NOT NULL DEFAULT '',
    cancel_reason TEXT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    confirmed_at TEXT NULL,
    cancelled_at TEXT NULL,
    FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE RESTRICT
);

INSERT INTO purchases_new (id, purchase_number, supplier_id, status, payment_type,
                           purchase_date, due_date, supplier_invoice_no, notes,
                           cancel_reason, created_by, updated_by,
                           created_at, updated_at, confirmed_at, cancelled_at)
SELECT id, purchase_number, supplier_id, status, payment_type,
       purchase_date, due_date, supplier_invoice_no, notes,
       cancel_reason,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at, updated_at, confirmed_at, cancelled_at
FROM purchases;

DROP TABLE purchases;
ALTER TABLE purchases_new RENAME TO purchases;

CREATE INDEX IF NOT EXISTS idx_purchases_status ON purchases(status);
CREATE INDEX IF NOT EXISTS idx_purchases_supplier_id ON purchases(supplier_id);
CREATE INDEX IF NOT EXISTS idx_purchases_purchase_date ON purchases(purchase_date);

-- ---------------------------------------------------------------------------
-- purchase_payments: rebuild with the audit columns. The live definition is
-- migration 17's plus the two transaction/refund links migration 19 ALTERed
-- on (both RESTRICT), so the CASCADE purchase link, the RESTRICT
-- account/method references and both link indexes are preserved as declared.
-- ---------------------------------------------------------------------------
CREATE TABLE purchase_payments_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    purchase_id INTEGER NOT NULL REFERENCES purchases(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    method_id INTEGER NOT NULL REFERENCES payment_methods(id) ON DELETE RESTRICT,
    amount TEXT NOT NULL,
    date TEXT NOT NULL,
    transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT,
    refund_transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (purchase_id) REFERENCES purchases(id) ON DELETE CASCADE,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE RESTRICT,
    FOREIGN KEY (method_id) REFERENCES payment_methods(id) ON DELETE RESTRICT
);

INSERT INTO purchase_payments_new (id, purchase_id, account_id, method_id, amount, date,
                                   transaction_id, refund_transaction_id,
                                   created_by, updated_by, created_at)
SELECT id, purchase_id, account_id, method_id, amount, date,
       transaction_id, refund_transaction_id,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM purchase_payments;

DROP TABLE purchase_payments;
ALTER TABLE purchase_payments_new RENAME TO purchase_payments;

CREATE INDEX IF NOT EXISTS idx_purchase_payments_purchase_id
    ON purchase_payments(purchase_id);
CREATE INDEX IF NOT EXISTS idx_purchase_payments_account_id
    ON purchase_payments(account_id);
CREATE INDEX IF NOT EXISTS idx_purchase_payments_method_id
    ON purchase_payments(method_id);
CREATE INDEX IF NOT EXISTS idx_purchase_payments_transaction_id
    ON purchase_payments(transaction_id);
CREATE INDEX IF NOT EXISTS idx_purchase_payments_refund_transaction_id
    ON purchase_payments(refund_transaction_id);

PRAGMA foreign_keys = ON;
