-- Stock movements: append-only history. Stock is derived SUM in Rust.
-- qty is input magnitude as TEXT (Adjust may carry a signed delta).
-- product_id uses ON DELETE RESTRICT so history is preserved.
CREATE TABLE IF NOT EXISTS stock_movements (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    qty TEXT NOT NULL,
    type TEXT NOT NULL CHECK (type IN ('In', 'Out', 'Adjust')),
    reason TEXT NOT NULL CHECK (reason IN ('Purchase', 'Sale', 'Loss', 'Adjust', 'Initial')),
    reference TEXT NOT NULL DEFAULT '',
    date TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_stock_movements_product_id ON stock_movements(product_id);
CREATE INDEX IF NOT EXISTS idx_stock_movements_product_date ON stock_movements(product_id, date);
