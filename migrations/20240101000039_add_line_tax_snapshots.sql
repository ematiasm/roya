-- Immutable per-line tax snapshots and the line tax total (tax calculation and
-- settings, T1).
--
-- TWO explicit tables, one per document family, instead of a polymorphic
-- `document_line_id` reference: `sale_lines` and `purchase_lines` are separate
-- tables with separate identities, and a per-family table keeps the foreign key
-- pointing at the exact parent it belongs to (SQLite cannot express "either
-- table" as one constraint).
--
-- A snapshot row is written ONCE, when the line is created, and is never
-- updated. It stores BOTH the tax reference (`tax_id`) and the facts the
-- calculation actually used (`tax_code`, `tax_name`, `rate`, `amount`), so a
-- later catalog edit or deactivation cannot rewrite the historical meaning of a
-- confirmed document. `rate` and `amount` are canonical decimal TEXT like every
-- other decimal in this project; no localized display value is ever persisted.
--
-- `tax_id` is ON DELETE RESTRICT: the application-level hard delete (T3)
-- rejects a referenced tax with an actionable conflict, and this edge remains
-- the database backstop. The line edge is ON DELETE CASCADE, so a draft line
-- that is edited away takes its snapshots with it instead of leaving rows that
-- reference a dead line.
--
-- WHERE THE LINE'S MONEY LIVES:
--   * `tax_total` (below) is STORED on the line: it is the aggregate the
--     document total, the payment limit and the debt calculation read, and
--     summing it in SQL is impossible over TEXT.
--   * The net subtotal stays DERIVED (`qty * unit_price`), exactly as before.
--   * The tax-inclusive line total stays DERIVED
--     (`round_half_up(net + tax_total, 2)`), like every other document total
--     in this project, which is never stored as truth.
-- `tax_total` is NOT NULL DEFAULT '0', which is truthful for every existing
-- row: a line created before this feature carried no tax, so its
-- tax-inclusive total is unchanged by this migration and confirmed documents
-- keep their financial meaning.
CREATE TABLE sale_line_taxes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sale_line_id INTEGER NOT NULL REFERENCES sale_lines(id) ON DELETE CASCADE,
    tax_id INTEGER NOT NULL REFERENCES taxes(id) ON DELETE RESTRICT,
    tax_code TEXT NOT NULL,
    tax_name TEXT NOT NULL,
    rate TEXT NOT NULL,
    amount TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (sale_line_id, tax_id)
);

-- The UNIQUE (sale_line_id, tax_id) index already serves reads by line, so
-- only the reverse direction needs its own index: the hard-delete safeguard
-- (T3) counts the document references of one tax.
CREATE INDEX idx_sale_line_taxes_tax_id ON sale_line_taxes(tax_id);

CREATE TABLE purchase_line_taxes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    purchase_line_id INTEGER NOT NULL REFERENCES purchase_lines(id) ON DELETE CASCADE,
    tax_id INTEGER NOT NULL REFERENCES taxes(id) ON DELETE RESTRICT,
    tax_code TEXT NOT NULL,
    tax_name TEXT NOT NULL,
    rate TEXT NOT NULL,
    amount TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (purchase_line_id, tax_id)
);

CREATE INDEX idx_purchase_line_taxes_tax_id ON purchase_line_taxes(tax_id);

ALTER TABLE sale_lines ADD COLUMN tax_total TEXT NOT NULL DEFAULT '0';
ALTER TABLE purchase_lines ADD COLUMN tax_total TEXT NOT NULL DEFAULT '0';
