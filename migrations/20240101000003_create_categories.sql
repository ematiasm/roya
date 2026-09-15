-- Create categories table (M1 inventory, free self-referencing tree).
-- Root = parent_id IS NULL. Duplicate root names are guarded at service
-- level because SQLite treats NULLs as distinct inside UNIQUE.
CREATE TABLE IF NOT EXISTS categories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    parent_id INTEGER NULL REFERENCES categories(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (id != parent_id),
    UNIQUE (parent_id, name),
    FOREIGN KEY (parent_id) REFERENCES categories(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_categories_parent_id ON categories(parent_id);
