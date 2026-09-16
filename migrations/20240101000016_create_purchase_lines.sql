-- Purchase lines (M3). Decimal qty (> 0) / unit_cost (>= 0) as TEXT like finance.
-- qty/unit_cost bounds are enforced by PurchasesService; SQLite TEXT cannot
-- compare decimals safely in a CHECK. product_id RESTRICT: history preserved.
-- purchase_id CASCADE: lines die with the purchase.
CREATE TABLE IF NOT EXISTS purchase_lines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    purchase_id INTEGER NOT NULL REFERENCES purchases(id) ON DELETE CASCADE,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    qty TEXT NOT NULL,
    unit_cost TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (purchase_id) REFERENCES purchases(id) ON DELETE CASCADE,
    FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_purchase_lines_purchase_id ON purchase_lines(purchase_id);
CREATE INDEX IF NOT EXISTS idx_purchase_lines_product_id ON purchase_lines(product_id);
