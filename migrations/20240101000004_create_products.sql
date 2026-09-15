-- Create products table (M1 inventory).
-- Decimal money/quantity columns use TEXT (same pattern as finance in SQLite).
CREATE TABLE IF NOT EXISTS products (
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
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (category_id) REFERENCES categories(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_products_category_id ON products(category_id);
CREATE INDEX IF NOT EXISTS idx_products_kind ON products(kind);
CREATE INDEX IF NOT EXISTS idx_products_is_active ON products(is_active);
