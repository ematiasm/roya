-- no-transaction
-- Audit on the inventory tables (M5 Phase B, slice S10): every row of
-- `categories`, `products` and `stock_movements` records who created it
-- (`created_by`, NOT NULL) and, when applicable, who last changed it
-- (`updated_by`, NULL). The columns are ordinary integers with a RESTRICT
-- foreign key to `users`; the owning module writes them from the `Principal`
-- each request resolves, so inventory never reads identity tables (spec
-- invariant 1, AC20). `product_barcodes` gains no columns: like the document
-- lines in the plan's Audit section, a join row inherits the actor of its
-- parent row (the product), so a barcode is as attributable as the product it
-- aliases without duplicating the columns.
--
-- Same technique as migration 30 (the finance audit) and, before it, migration
-- 21 (`add_sales_customer`) and migration 24 (`payment_methods_single_account`):
-- the columns cannot be `ALTER TABLE`d into existence (SQLite refuses a NOT NULL
-- column without a constant default, and a `REFERENCES` clause needs the table
-- rebuild anyway), and sqlx runs migrations inside a transaction where
-- PRAGMA foreign_keys is a no-op and a deferred violation from DROP TABLE
-- cannot be healed before COMMIT. So this migration is marked
-- `-- no-transaction` and disables foreign keys itself for the swap,
-- re-enabling after. Dropping a rebuilt parent (categories is the parent of
-- products and of itself; products is the parent of stock_movements and
-- product_barcodes) would otherwise fire RESTRICT refusals or CASCADE
-- deletions; with keys off the drop is inert, child rows are untouched, and
-- every id is preserved by the INSERT ... SELECT, so the re-enabled
-- constraints find the same graph that existed before.
--
-- The actor for pre-existing rows: the sentinel migration 30 created. THIS
-- MIGRATION REUSES IT — on every database the chain builds the sentinel is
-- already here, because migration 12 seeds the five payment methods and
-- migration 30 therefore always finds finance rows to attribute and inserts
-- the account before this file runs. The guarded insert below is DEFENSIVE:
-- it fires only if the sentinel is somehow absent when this migration runs
-- (a database built outside the chain whose finance tables were emptied, for
-- example), and only when there is something to attribute — a database with
-- none of the three inventory tables populated gains no user at all. The
-- attribution rule is the same honest one as S9 (see migration 30's header
-- and openspec/changes/2026-09-19-add-actor-audit/spec.md): rows that predate
-- the audit were not created by any person the system knew, so they are
-- attributed to the inactive, roleless account whose stored hash is
-- deliberately malformed — never to a person who did not create them.
--
-- Idempotent guards (same discipline as migrations 27 and 30): the sentinel
-- insert is guarded with WHERE NOT EXISTS, so replaying the statement cannot
-- duplicate the row; if an operator already owns a user named `sistema`,
-- their row is the attribution target and no second one is created.

PRAGMA foreign_keys = OFF;

INSERT INTO users (username, display_name, password_hash, is_active)
SELECT 'sistema',
       'Sistema (anterior al registro)',
       -- Malformed on purpose: `PasswordVerifier` answers false for a hash it
       -- cannot parse (tested in security/password.rs), and the account is
       -- inactive on top, so no credential can ever log it in. Byte-identical
       -- to migration 30's sentinel so the two statements cannot drift apart.
       '$sentinel$no-login-credential$',
       0
WHERE NOT EXISTS (SELECT 1 FROM users WHERE username = 'sistema' COLLATE NOCASE)
  AND ( EXISTS (SELECT 1 FROM categories)
     OR EXISTS (SELECT 1 FROM products)
     OR EXISTS (SELECT 1 FROM stock_movements) );

-- ---------------------------------------------------------------------------
-- categories: rebuild with the audit columns. The self-referencing tree
-- (parent_id RESTRICT, the `id != parent_id` check and the UNIQUE(parent_id,
-- name) rule) is preserved declaration-for-declaration, so the service's
-- tree guards keep their database backstop.
-- ---------------------------------------------------------------------------
CREATE TABLE categories_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    parent_id INTEGER NULL REFERENCES categories(id) ON DELETE RESTRICT,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (id != parent_id),
    UNIQUE (parent_id, name),
    FOREIGN KEY (parent_id) REFERENCES categories(id) ON DELETE RESTRICT
);

INSERT INTO categories_new (id, name, parent_id, created_by, updated_by, created_at)
SELECT id, name, parent_id,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM categories;

DROP TABLE categories;
ALTER TABLE categories_new RENAME TO categories;

CREATE INDEX IF NOT EXISTS idx_categories_parent_id ON categories(parent_id);

-- ---------------------------------------------------------------------------
-- products: rebuild with the audit columns. The SKU uniqueness, the kind and
-- is_active checks, the category SET NULL and every id are preserved, so
-- barcodes, stock movements and sale/purchase lines keep their product ids.
-- ---------------------------------------------------------------------------
CREATE TABLE products_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sku TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('Product', 'Service')),
    category_id INTEGER NULL REFERENCES categories(id) ON DELETE SET NULL,
    unit TEXT NOT NULL,
    sale_price TEXT NOT NULL,
    cost_price TEXT NOT NULL DEFAULT '0',
    track_stock INTEGER NOT NULL CHECK (track_stock IN (0, 1)),
    min_stock TEXT NULL,
    max_stock TEXT NULL,
    location TEXT NULL,
    notes TEXT NULL,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (category_id) REFERENCES categories(id) ON DELETE SET NULL
);

INSERT INTO products_new (id, sku, name, kind, category_id, unit, sale_price, cost_price,
                          track_stock, min_stock, max_stock, location, notes, is_active,
                          created_by, updated_by, created_at, updated_at)
SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price,
       track_stock, min_stock, max_stock, location, notes, is_active,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at, updated_at
FROM products;

DROP TABLE products;
ALTER TABLE products_new RENAME TO products;

CREATE INDEX IF NOT EXISTS idx_products_category_id ON products(category_id);
CREATE INDEX IF NOT EXISTS idx_products_kind ON products(kind);
CREATE INDEX IF NOT EXISTS idx_products_is_active ON products(is_active);

-- ---------------------------------------------------------------------------
-- stock_movements: rebuild with the audit columns. The reason CHECK is the
-- EXPANDED one migrations 11 and 18 produced ('Sale-return' and
-- 'Purchase-return' added to the original five) — the rebuild must carry the
-- live constraint, not the migration-6 original. Append-only history is
-- preserved: every id, and now every movement's actor.
-- ---------------------------------------------------------------------------
CREATE TABLE stock_movements_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    qty TEXT NOT NULL,
    type TEXT NOT NULL CHECK (type IN ('In', 'Out', 'Adjust')),
    reason TEXT NOT NULL CHECK (reason IN ('Purchase', 'Sale', 'Sale-return', 'Purchase-return', 'Loss', 'Adjust', 'Initial')),
    reference TEXT NOT NULL DEFAULT '',
    date TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT
);

INSERT INTO stock_movements_new (id, product_id, qty, type, reason, reference, date, created_by, updated_by, created_at)
SELECT id, product_id, qty, type, reason, reference, date,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM stock_movements;

DROP TABLE stock_movements;
ALTER TABLE stock_movements_new RENAME TO stock_movements;

CREATE INDEX IF NOT EXISTS idx_stock_movements_product_id ON stock_movements(product_id);
CREATE INDEX IF NOT EXISTS idx_stock_movements_product_date ON stock_movements(product_id, date);

PRAGMA foreign_keys = ON;
