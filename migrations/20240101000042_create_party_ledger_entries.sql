-- The party ledger: ONE signed entry table per party (customer, supplier), the
-- single source of truth for every party balance in the app — outstanding debt,
-- saldo a favor, ageing, credit limit, payables, the account statement.
-- (odd/tasks/party-ledger.md, decisions 1, 3, 7, 9.)
--
-- WHY ONE TABLE AND NOT FOUR: the sign rule is identical for both party types
-- and for every event, so four family-shaped tables would be four copies of one
-- fold. A row says who (`party_type`, `party_id`), what happened (`kind`), how
-- much and in which DIRECTION (`amount`), and which document says so
-- (`document_kind`, `document_id`, `reference`).
--
-- THE SIGN RULE, written here because it is the one thing no reader of this
-- schema can infer (decision 1):
--     amount > 0  = outstanding obligation: the customer owes the business, or
--                   the business owes the supplier.
--     amount < 0  = saldo a favor (a credit balance).
--     kind: Charge  +total   document confirmed (cash or credit alike)
--           Payment −amount cash settled against the document
--           Return  −total  goods returned / credit note
--           Refund  +amount cash handed back to the party
--           Cancel  −total  whole document annulled
-- A negative balance is a LEGAL RESULT of this fold (overpayment is a credit,
-- not an error), so nothing here may treat `amount < 0` as invalid, and no
-- writer may clamp it. The sign is stored, never re-derived at read time: the
-- read is one checked sum over `amount`, so a second sign function would be a
-- second opinion about the same rows.
--
-- NO FOREIGN KEY ON `party_id`, deliberately: SQLite cannot express "either
-- customers or suppliers" as one constraint — the value's meaning is decided by
-- `party_type`, and an FK on either table would be wrong for half the rows. The
-- same argument is why `document_kind`/`document_id` is a (kind, id) PAIR and
-- not a REFERENCES edge: four document families, one column set (this is the
-- polymorphic reference migration 39 rejected for line taxes, where a real
-- per-parent edge existed to keep — here the whole point is that one table
-- points at four).
--
-- APPEND-ONLY (decision 3): the journal never deletes and never updates an
-- entry after creation; a cancelled document appends its own `Cancel`/`Refund`
-- reversal. `updated_by`/`updated_at` exist for the schema's uniformity and for
-- a future correction path, not because a balance row is edited in place — a
-- balance that changed because a row was rewritten cannot be audited.
--
-- WHY THIS MIGRATION CARRIES NO SEED (decision 9): a document total is
-- DERIVED — `round_half_up(qty * unit_price + tax_total, 2)` per line
-- (migration 39) — and SQLite arithmetic over TEXT decimals silently runs in
-- REAL, which is the floating point this project forbids for money. So the
-- backfill is a Rust function (`repositories::party_ledger_repo::
-- backfill_party_ledger`) called from `db.rs` immediately after
-- `sqlx::migrate!`, inside ONE transaction, guarded by an empty table. On a
-- fresh database both the migration and the backfill are no-ops over data.
--
-- `entry_date` is the event's date as the DOCUMENT wrote it (`sale_date`,
-- `purchase_date`, `return_date`, a payment's `date`): `YYYY-MM-DD` TEXT, the
-- same lexical shape every other date column here stores, and a prefix of the
-- instant shape `crate::db::encode_sqlite_timestamp` produces, so a date
-- compares correctly against a DB-written instant in the same column. Ordering
-- within a party is by `id` (see the index below), never by this column, so a
-- same-day pair keeps its write order.
CREATE TABLE IF NOT EXISTS party_ledger_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    party_type TEXT NOT NULL,      -- 'Customer' | 'Supplier'
    party_id INTEGER NOT NULL,     -- no FK: SQLite cannot express "either customers or suppliers"
    kind TEXT NOT NULL,            -- 'Charge' | 'Payment' | 'Return' | 'Refund' | 'Cancel'
    amount TEXT NOT NULL,          -- signed Decimal; >0 = outstanding obligation, <0 = saldo a favor
    document_kind TEXT NOT NULL,   -- 'Sale' | 'Purchase' | 'CustomerReturn' | 'PurchaseReturn'
    document_id INTEGER NOT NULL,
    entry_date TEXT NOT NULL,      -- lexical-safe: use crate::db::encode_sqlite_timestamp semantics for generated values
    reference TEXT,                -- document number (opaque)
    created_by INTEGER NOT NULL,
    updated_by INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- The balance fold reads one party's rows and folds them in `id` order (the
-- order the write-side pre-check folds in, and the order that decides which
-- prefixes the checked sum sees). `(party_type, party_id, id)` serves both the
-- filter and that sort with no separate sort pass — `id` last is what makes the
-- index-ordered scan the fold's own order.
CREATE INDEX IF NOT EXISTS idx_party_ledger_party ON party_ledger_entries(party_type, party_id, id);

-- The statement/audit read: everything written against one document, including
-- the reversal rows a cancel appends and the payments that settled it.
CREATE INDEX IF NOT EXISTS idx_party_ledger_document ON party_ledger_entries(document_kind, document_id);
