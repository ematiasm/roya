-- Migration 46: the `payments` family — one document per DELIVERY of money, and
-- the explicit allocations that say which documents it pays.
--
-- Decision 1 of odd/tasks/payment-allocation.md: ONE family for both parties,
-- with a `direction`. `'In'` is money received (a customer collection, however it
-- arrives), `'Out'` is money handed over (a supplier payment, or a refund to a
-- customer). Customers and suppliers share the shape because the business fact is
-- the same: somebody handed over an amount, it covers one or several documents,
-- and it moves cash exactly once. The `number` comes from `doc_sequences`, the
-- same no-gap mechanism that issues `sale_number` and `credit_note_number`, which
-- is what makes a delivery of money citable for the first time — `customer_receipts`
-- never had a number of its own.
--
-- Decision 5: `payments.transaction_id` is the SINGLE `transactions` row, with
-- `reference` = the payment number. Allocations deliberately carry no account, no
-- method and no transaction: the money moved when it arrived, and applying it to a
-- document later is only an allocation. That is what makes an applied credit
-- representable without inventing a movement that never happened.
--
-- Decision 7: `payments` inherits migration 44's method→account guard, because it
-- carries the same pair for the same reason. The trigger below is 44's text with
-- the table name changed, and the same two properties hold: insert-only (the
-- stored account is where the money landed, a historical fact that must not move
-- when a method is re-pointed) and `COALESCE(…, -1)` so a method that does not
-- exist is refused by the trigger even where `PRAGMA foreign_keys` is off.
--
-- ---------------------------------------------------------------------------
-- THE SPLIT CAP, in its two homes (decision 4, refined 2026-10-08)
-- ---------------------------------------------------------------------------
-- `unapplied = delivered − allocated` is the number the business reads as the
-- credit balance, so it must never be able to go negative. The cap that keeps it
-- honest has TWO homes on purpose, and the reasoning is measured rather than
-- argued:
--
--   1. THE SERVICE is the real gate. It reads a payment's allocations with an
--      `_in` method inside the caller's unit, folds them in `Decimal` with the
--      existing `checked_money_sum`, and refuses with `AppError::Validation`,
--      because the service sees the WHOLE pair (the payment and its allocations)
--      and SQLite triggers cannot.
--   2. THIS TRIGGER is the backstop against a hand-written INSERT. It is
--      INSERT-only on purpose: the cap is enforced the moment a share is created,
--      and an UPDATE that keeps the set inside the cap is the service's business.
--
-- Why the trigger cannot also cover UPDATE, stated so nobody "fixes" it later:
-- a BEFORE UPDATE twin would fire for every reallocation the operator legitimately
-- makes, and the one case it would catch that the insert does not — lowering the
-- PAYMENT's amount under its allocations — lives on ANOTHER TABLE and is not
-- visible here at all. Measured on SQLite 3.53.4: 70 allocated against a payment
-- of 100, then `UPDATE payments SET amount = '10'`, and nothing errors anywhere.
-- That hole is closed on the document instead of here: a payment's `amount` is
-- immutable once it has allocations, so the residual can never be driven negative
-- by an edit. See the guard below.
--
-- The arithmetic is the other measured limit: SQLite cannot fold TEXT decimals, so
-- the comparison below needs `CAST(… AS REAL)` and `REAL` is NOT the arithmetic
-- the code folds (`0.10 − 0.09` is `0.010000000000000009`). With round cents the
-- verdict agrees; at an exact boundary it is a different sum. This project forbids
-- REAL for money, which is exactly why this trigger is a NET and not the rule: it
-- can refuse a legitimate boundary case, and a refusal here is a hand-written
-- statement being told to use the service instead. It must never be the only
-- check, and no code path may rely on it for the message the operator sees.

PRAGMA foreign_keys = ON;

CREATE TABLE payments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Human-citable document number from `doc_sequences`, format `YYYY-PAY-NNNNNN`.
    number TEXT NOT NULL UNIQUE,
    direction TEXT NOT NULL CHECK (direction IN ('In', 'Out')),
    -- The party. Polymorphic by design (a payment belongs to a customer OR a
    -- supplier), so there is no foreign key: `party_type` decides which table
    -- `party_id` names, and the CHECK below keeps the type closed.
    party_type TEXT NOT NULL CHECK (party_type IN ('Customer', 'Supplier')),
    party_id INTEGER NOT NULL,
    -- The pair inherited from migration 44's guard. NOT NULL because a payment
    -- always moved through a real method into a real account.
    method_id INTEGER NOT NULL REFERENCES payment_methods(id) ON DELETE RESTRICT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    amount TEXT NOT NULL,
    date TEXT NOT NULL,
    notes TEXT NULL,
    -- Decision 5: the ONE cash movement. Nullable because the row is created
    -- before finance is touched — but a committed payment has it, and the
    -- service writes both inside one unit.
    transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_payments_party ON payments(party_type, party_id);
CREATE INDEX idx_payments_date ON payments(date);
CREATE INDEX idx_payments_direction ON payments(direction);

-- ---------------------------------------------------------------------------
-- `payment_allocations`: one row per (payment × target document), decision 3.
--
-- MUTABLE state, not a journal. An allocation folds into no balance: it is
-- subtracted from a residual that is always recomputed, and its total is bounded
-- by the payment document. The party ledger's append-only discipline exists
-- because there the balance IS the fold of its rows; that argument does not
-- transfer here, so mutability puts no total at risk while letting the operator
-- correct an attribution. Hence `updated_by`/`updated_at` rather than triggers
-- that abort an UPDATE.
--
-- A target is polymorphic for the same reason a party is: a payment can cover a
-- `Sale`, a `Purchase`, or (for an out-payment) one of the return families. There
-- is no foreign key, and `UNIQUE` below is what makes the pair addressable.
-- ---------------------------------------------------------------------------
CREATE TABLE payment_allocations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    payment_id INTEGER NOT NULL REFERENCES payments(id) ON DELETE CASCADE,
    target_kind TEXT NOT NULL CHECK (
        target_kind IN ('Sale', 'Purchase', 'CustomerReturn', 'PurchaseReturn')
    ),
    target_id INTEGER NOT NULL,
    -- Positive, like every stored magnitude in this schema: the DIRECTION lives on
    -- the payment, never in the sign of an allocation.
    amount TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- One share per (payment, document). Reallocating is an UPDATE of this row;
    -- naming the same document twice would be two shares of one residual, which
    -- has no meaning.
    UNIQUE (payment_id, target_kind, target_id)
);

CREATE INDEX idx_payment_allocations_payment ON payment_allocations(payment_id);
CREATE INDEX idx_payment_allocations_target ON payment_allocations(target_kind, target_id);

-- The positive-amount rule, in the schema rather than in a caller: `amount` is
-- TEXT so a CHECK cannot cast it, and a NON-NUMERIC string compares as 0 in
-- SQLite — so the guard is "not zero and not negative-looking", and the service
-- is what parses the Decimal. Written as a trigger because a CHECK cannot read
-- TEXT as a number.
CREATE TRIGGER trg_payment_allocations_amount_positive_on_insert
BEFORE INSERT ON payment_allocations
FOR EACH ROW
WHEN CAST(NEW.amount AS REAL) <= 0
BEGIN
    SELECT RAISE(ABORT, 'an allocation amount must be positive');
END;

CREATE TRIGGER trg_payment_allocations_amount_positive_on_update
BEFORE UPDATE OF amount ON payment_allocations
FOR EACH ROW
WHEN CAST(NEW.amount AS REAL) <= 0
BEGIN
    SELECT RAISE(ABORT, 'an allocation amount must be positive');
END;

-- ---------------------------------------------------------------------------
-- The split cap, home 2 of 2 (see the header). INSERT-only, and the `id <> NEW.id`
-- clause is absent because there is no UPDATE twin to need it.
-- ---------------------------------------------------------------------------
CREATE TRIGGER trg_payment_allocations_cap_on_insert
BEFORE INSERT ON payment_allocations
FOR EACH ROW
WHEN (
        SELECT COALESCE(SUM(CAST(amount AS REAL)), 0)
          FROM payment_allocations
         WHERE payment_id = NEW.payment_id
     ) + CAST(NEW.amount AS REAL)
     > (
        SELECT CAST(amount AS REAL) FROM payments WHERE id = NEW.payment_id
     )
BEGIN
    SELECT RAISE(ABORT, 'the allocations of a payment cannot exceed its amount');
END;

-- ---------------------------------------------------------------------------
-- Migration 44's guard, inherited by the new table (decision 7). Same text, same
-- INSERT-only asymmetry, same COALESCE sentinel.
-- ---------------------------------------------------------------------------
CREATE TRIGGER trg_payments_method_account_insert
BEFORE INSERT ON payments
FOR EACH ROW
WHEN NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;

-- ---------------------------------------------------------------------------
-- The immutability that closes the cap's blind spot (decision 4, refined).
--
-- A payment's `amount` cannot change once ANY allocation exists. Measured: the
-- cap trigger cannot see this UPDATE (it lives on the parent table), so lowering
-- the delivered figure under its own allocations — 70 allocated, payment set to
-- 10 — left `unapplied` negative with no error anywhere, and `unapplied` is the
-- number the business reads as the credit balance.
--
-- Two details that are the point rather than the plumbing:
--
--   * It fires on UPDATE OF amount, so an ordinary edit of `notes` or `date` is
--     not blocked by a rule about money.
--   * It only blocks a payment that HAS allocations. A payment with none has no
--     residual to protect, and correcting a typo before allocating anything is
--     exactly what an operator should be able to do. The rule is "what you
--     delivered is settled once you have said where it went", not "the row is
--     frozen the moment it is created".
--
-- Changing the delivered figure after allocating is a re-issue: reverse the
-- payment and record a new one, so the journal shows both facts instead of one
-- document whose amount and shares disagree.
-- ---------------------------------------------------------------------------
CREATE TRIGGER trg_payments_amount_immutable_once_allocated
BEFORE UPDATE OF amount ON payments
FOR EACH ROW
WHEN NEW.amount <> OLD.amount
 AND EXISTS (SELECT 1 FROM payment_allocations WHERE payment_id = OLD.id)
BEGIN
    SELECT RAISE(ABORT, 'a payment amount cannot change once it has allocations');
END;

-- NO `updated_at` TRIGGERS HERE, and that is a deliberate refusal.
--
-- The obvious move is a `BEFORE UPDATE` twin of migration 36's
-- `trg_payment_methods_set_updated_at`. Writing it correctly first requires
-- knowing that migration 36's idiom does NOT work:
--
--     BEGIN SELECT NEW.updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'); END;
--
-- is a comparison whose result is discarded, so the trigger has never stamped
-- anything on any table it was installed on. That was measured while closing T6 of
-- `odd/tasks/payment-method-single-account.md`, and after a table rebuild the
-- broken trigger was replaced by a column DEFAULT because no trigger shape is
-- honest to a `RETURNING` reader anyway.
--
-- So these tables get the SAME shape every other repository here already uses: the
-- UPDATE statement names `updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`
-- itself, which is what `payment_method_repo.rs`, `sale_repo.rs` and the rest do.
-- A trigger would be a second home for one rule, and the one idiom available for
-- it is the idiom that silently does nothing.
