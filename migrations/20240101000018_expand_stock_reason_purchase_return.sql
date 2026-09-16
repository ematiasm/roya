-- M3 purchases need 'Purchase-return' reason for stock Out movements when a
-- Confirmed purchase is cancelled and the goods go back to the supplier.
-- SQLite has no ALTER CHECK, so rebuild stock_movements with expanded CHECK.
-- Existing rows are preserved; new CHECK is a superset of the old one.
CREATE TABLE IF NOT EXISTS stock_movements_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    qty TEXT NOT NULL,
    type TEXT NOT NULL CHECK (type IN ('In', 'Out', 'Adjust')),
    reason TEXT NOT NULL CHECK (reason IN ('Purchase', 'Sale', 'Sale-return', 'Purchase-return', 'Loss', 'Adjust', 'Initial')),
    reference TEXT NOT NULL DEFAULT '',
    date TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT
);

INSERT OR IGNORE INTO stock_movements_new (id, product_id, qty, type, reason, reference, date, created_at)
    SELECT id, product_id, qty, type, reason, reference, date, created_at FROM stock_movements;

DROP TABLE IF EXISTS stock_movements;

ALTER TABLE stock_movements_new RENAME TO stock_movements;

CREATE INDEX IF NOT EXISTS idx_stock_movements_product_id ON stock_movements(product_id);
CREATE INDEX IF NOT EXISTS idx_stock_movements_product_date ON stock_movements(product_id, date);
