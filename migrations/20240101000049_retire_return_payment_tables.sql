-- Migration 49: the two return-payment tables are gone. Both are empty.
--
-- Why they existed. A credit note's refund and a purchase return's refund each
-- kept their own satellite table (`customer_return_payments`,
-- `purchase_return_payments`), one row per money movement, exactly like the
-- document families above them. They were the third home of one fact: the
-- refund, its account, and its method.
--
-- Why they are gone. Decision 9 of the payment-allocation work made a refund a
-- PAYMENT with `direction = 'Out'` for a customer and `'In'` for a supplier, so
-- the refund's money already lives in `payments` + `payment_allocations` and its
-- single cash movement in `transactions`. P5.3c deleted the writes; P5.3b-4 and
-- P5.3b-5 moved the reads (the details, the refund plans, the reversal guard and
-- the party-page reads all resolve `payments` rows now); P8-1 deleted the ledger
-- backfill, which was the last SELECT over either table, so `party_ledger_repo`
-- no longer names them at all. A grep over `src/` now finds these two names only
-- in comments.
--
-- Emptiness is asserted, not assumed. Both tables are checked to hold zero rows
-- BEFORE the drop, with the same `CHECK (count = 0)` shape migration 45 used for
-- its own guard: a silent drop of rows is the failure mode this migration would
-- otherwise hide, and a maintenance branch that still had rows would lose money
-- history without a word. The counter is a temp table rather than a bare
-- `SELECT` because SQLite executes a `CHECK` against a row that must exist —
-- a bare count is discarded, exactly like the `BEGIN SELECT NEW.x = ...` idiom
-- migration 36 fell into and migration 45 removed.
--
-- No `-- no-transaction` door, and no table rebuild. Measured on a
-- freshly-migrated database: these two tables carry NO triggers of their own
-- (unlike `payment_methods`, whose rebuild in 45 needed the child triggers
-- dropped first), no index beyond their own, and nothing outside them declares a
-- foreign key that points at them. So the drop is the whole change.

PRAGMA foreign_keys = ON;

CREATE TEMP TABLE temp_retired_return_payments_guard (rows_left INTEGER NOT NULL);
INSERT INTO temp_retired_return_payments_guard (rows_left)
SELECT (SELECT COUNT(*) FROM customer_return_payments)
     + (SELECT COUNT(*) FROM purchase_return_payments);
CREATE TEMP TABLE temp_retired_return_payments_check (
    violations INTEGER NOT NULL
    CHECK (violations = 0)
);
INSERT INTO temp_retired_return_payments_check (violations)
SELECT rows_left FROM temp_retired_return_payments_guard;
DROP TABLE temp_retired_return_payments_guard;
DROP TABLE temp_retired_return_payments_check;

DROP TABLE customer_return_payments;
DROP TABLE purchase_return_payments;
