-- Migration 44: the (account_id, method_id) pair on a payment row is guarded
-- by the schema WHERE A PAIR IS CHOSEN.
--
-- The gap. Every collection path resolves the account FROM the method
-- (`resolve_method_account`) and then writes both columns, so on the three
-- method-choosing tables (`sale_payments`, `purchase_payments`,
-- `customer_receipts`) the pair was consistent by caller discipline only: no
-- constraint, index or trigger relates `account_id` to `method_id`, and a
-- direct SQL insert can name any account (or, via `sale_payments.method_id
-- DEFAULT 1`, silently pair with method 1, whose owner may be a different
-- account). These three triggers close that hole.
--
-- Scope, deliberately narrowed: the two refund tables
-- (`customer_return_payments`, `purchase_return_payments`) are EXEMPT. A
-- refund does not choose a pair — it REPLAYS the parent payment's pair so the
-- money comes back out of the box it went into (`RefundPlan` copies
-- pay.account_id / pay.method_id by design). When a method is re-pointed
-- (`set_method_account` / `replace_account_methods`), a legitimate refund row
-- is born mismatched against the method's CURRENT owner, and a guard here
-- would abort a valid operation: a hardening that changes behaviour for a
-- valid operation is wrong, not the operation. The residual hole is accepted
-- and named: a direct SQL insert on those two tables can still name any
-- account, and their writers copy a pair from a parent row that was itself
-- guarded at birth. Rejected alternative: making refunds resolve the account
-- from the method's current owner would close the hole but silently change
-- WHICH box funds a refund — a product decision, not a hardening side effect.
--
-- Deliberate asymmetry: INSERT-only, no BEFORE UPDATE twin. Equality is
-- enforced when the row is born; afterwards the stored account is the
-- historical fact of where the money actually landed, while the method is
-- mutable configuration. If an owner re-points a method to another account,
-- history must not move (the refund paths already read the stored account for
-- exactly that reason), so divergence after birth is legitimate and must not
-- be blocked.
--
-- `COALESCE(…, -1)` is load-bearing, not decoration. An unassigned method
-- (`payment_methods.account_id IS NULL`) makes the subquery NULL, the `<>`
-- NULL, and `WHEN NULL` means no abort — the exact case this guard exists to
-- stop would pass silently. The -1 sentinel also refuses a `method_id` that
-- does not exist at all, which makes the trigger stronger than the foreign
-- key: it holds even where `PRAGMA foreign_keys` is off. The schema is the
-- only home of the rule; no caller-side duplicate.

CREATE TRIGGER IF NOT EXISTS trg_sale_payments_method_account_insert
BEFORE INSERT ON sale_payments
FOR EACH ROW
WHEN NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;

CREATE TRIGGER IF NOT EXISTS trg_purchase_payments_method_account_insert
BEFORE INSERT ON purchase_payments
FOR EACH ROW
WHEN NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;

CREATE TRIGGER IF NOT EXISTS trg_customer_receipts_method_account_insert
BEFORE INSERT ON customer_receipts
FOR EACH ROW
WHEN NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;
