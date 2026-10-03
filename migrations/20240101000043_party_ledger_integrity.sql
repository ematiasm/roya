-- The party ledger's two integrity rules, enforced by the schema instead of
-- remembered by convention (odd/tasks/party-ledger.md, decisions 10 and 11).
--
-- 1. APPEND-ONLY (decision 10): BEFORE UPDATE and BEFORE DELETE triggers abort
--    every rewrite of the journal with the trigger's own message (decision 3
--    was prose until now). A correction is a reversal APPENDED, never an entry
--    edited in place, so the balance fold cannot drift from what happened.
--
-- 2. THE SINGLE-INSTANCE GUARD (decision 11): a partial unique index makes a
--    duplicated confirmation, return or cancellation a database error instead
--    of a silent balance change — the cheapest idempotency key a ledger is
--    expected to carry. `Payment` and `Refund` are deliberately OUTSIDE it:
--    several of each legitimately settle one document. Kind values are the
--    capitalized `PartyEntryKind::Display` spellings the write side stores.
--
-- THE VESTIGIAL COLUMNS GO LAST-ISH, NOT WITH THE INDEX: this migration also
-- drops `updated_by` and `updated_at`, SUPERSEDING migration 42's remark that
-- they exist "for a future correction path" — a correction is a reversal entry
-- (decision 3), so a timestamp nobody may write is dead schema inviting the
-- very mutation the triggers above refuse. Migration 42 itself is left
-- byte-identical: sqlx::migrate! verifies the checksum of every applied
-- migration, so editing it — even a comment — breaks every existing database
-- at startup.
--
-- ORDER MATTERS: the two DROP COLUMN statements come BEFORE the trigger
-- definitions. SQLite refuses to drop a column a trigger's body references
-- ("error in table party_ledger_entries after drop column: no such column:
-- updated_by" at UPDATE time); the triggers must not name the dropped columns,
-- and the drops must happen while no trigger on the table asks for them.

ALTER TABLE party_ledger_entries DROP COLUMN updated_at;
ALTER TABLE party_ledger_entries DROP COLUMN updated_by;

CREATE UNIQUE INDEX IF NOT EXISTS idx_party_ledger_single_kinds
  ON party_ledger_entries(document_kind, document_id, kind)
  WHERE kind IN ('Charge', 'Return', 'Cancel');

CREATE TRIGGER IF NOT EXISTS trg_party_ledger_entries_no_update
BEFORE UPDATE ON party_ledger_entries
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'party ledger entries are append-only: an entry cannot be updated');
END;

CREATE TRIGGER IF NOT EXISTS trg_party_ledger_entries_no_delete
BEFORE DELETE ON party_ledger_entries
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'party ledger entries are append-only: an entry cannot be deleted');
END;
