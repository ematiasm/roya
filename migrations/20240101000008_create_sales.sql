-- Sales header (M2 Ventas, orchestrator).
-- sale_number UNIQUE NULL: NULL in Draft / Cancelled-from-Draft, set on confirm
-- as YYYY-SALE-NNNNNN. SQLite treats NULLs as distinct in UNIQUE, so multiple
-- Drafts with NULL are allowed.
-- No sale-level account_id: each payment carries its account.
CREATE TABLE IF NOT EXISTS sales (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sale_number TEXT NULL UNIQUE,
    status TEXT NOT NULL CHECK (status IN ('Draft', 'Confirmed', 'Cancelled')),
    payment_type TEXT NOT NULL CHECK (payment_type IN ('Cash', 'Credit')),
    customer_name TEXT NOT NULL DEFAULT '',
    sale_date TEXT NOT NULL,
    due_date TEXT NULL,
    receipt_no TEXT NULL UNIQUE,
    notes TEXT NOT NULL DEFAULT '',
    cancel_reason TEXT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    confirmed_at TEXT NULL,
    cancelled_at TEXT NULL
);

CREATE INDEX IF NOT EXISTS idx_sales_status ON sales(status);
CREATE INDEX IF NOT EXISTS idx_sales_sale_date ON sales(sale_date);
