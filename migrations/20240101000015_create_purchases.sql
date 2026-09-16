-- Purchases header (M3 Compras, mirror orchestrator of M2 sales).
-- purchase_number UNIQUE NULL: NULL only while Draft, assigned on confirm as
-- YYYY-PURCH-NNNNNN via the doc_sequences consumer PURCH. SQLite treats NULLs
-- as distinct in UNIQUE, so multiple Drafts with NULL are allowed.
-- supplier_id NOT NULL RESTRICT: history survives, deactivate the supplier.
-- No purchase-level account_id: each payment carries its own account.
CREATE TABLE IF NOT EXISTS purchases (
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
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    confirmed_at TEXT NULL,
    cancelled_at TEXT NULL,
    FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_purchases_status ON purchases(status);
CREATE INDEX IF NOT EXISTS idx_purchases_supplier_id ON purchases(supplier_id);
CREATE INDEX IF NOT EXISTS idx_purchases_purchase_date ON purchases(purchase_date);
