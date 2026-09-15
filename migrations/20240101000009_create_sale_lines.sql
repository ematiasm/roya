-- Sale lines (M2). Decimal qty/unit_price as TEXT like finance.
-- product_id RESTRICT: history preserved. sale_id CASCADE: lines die with sale.
CREATE TABLE IF NOT EXISTS sale_lines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sale_id INTEGER NOT NULL REFERENCES sales(id) ON DELETE CASCADE,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    qty TEXT NOT NULL,
    unit_price TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (sale_id) REFERENCES sales(id) ON DELETE CASCADE,
    FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_sale_lines_sale_id ON sale_lines(sale_id);
CREATE INDEX IF NOT EXISTS idx_sale_lines_product_id ON sale_lines(product_id);
