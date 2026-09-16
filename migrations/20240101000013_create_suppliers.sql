-- Suppliers (M3 purchases). Real table (unlike customers): orders and history
-- reference it. name UNIQUE (service trims/validates non-empty <= 128); phone
-- and notes are optional free text. product_supplier_costs and future purchases
-- reference suppliers with RESTRICT, so history survives: deactivate with
-- is_active = 0 instead of deleting.
CREATE TABLE IF NOT EXISTS suppliers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    phone TEXT NULL,
    notes TEXT NULL,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_suppliers_is_active ON suppliers(is_active);
