-- Product/supplier cost satellite (M3). One row per (product, supplier) holding
-- the current price and the previous one with their dates (Decimal-as-TEXT like
-- the rest of the project). The "supplier raised the price" alert is derived
-- from previous vs current, never stored. product_id/supplier_id RESTRICT so
-- history survives. At most one is_preferred = 1 per product, enforced by the
-- partial unique index below.
CREATE TABLE IF NOT EXISTS product_supplier_costs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    supplier_id INTEGER NOT NULL REFERENCES suppliers(id) ON DELETE RESTRICT,
    current_cost TEXT NOT NULL,
    current_cost_updated_at TEXT NOT NULL,
    previous_cost TEXT NULL,
    previous_cost_updated_at TEXT NULL,
    is_preferred INTEGER NOT NULL DEFAULT 0 CHECK (is_preferred IN (0, 1)),
    supplier_sku TEXT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (product_id, supplier_id),
    FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT,
    FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_product_supplier_costs_product_id
    ON product_supplier_costs(product_id);
CREATE INDEX IF NOT EXISTS idx_product_supplier_costs_supplier_id
    ON product_supplier_costs(supplier_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_product_supplier_costs_one_preferred
    ON product_supplier_costs(product_id) WHERE is_preferred = 1;
