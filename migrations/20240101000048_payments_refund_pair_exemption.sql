-- Migration 48: decision 9 requires refunds to remain Out payments for
-- customers and In payments for suppliers, replaying the account/method pair
-- stored on the parent payment even if the method has since been re-pointed.
-- These are exactly the two refund pairs that replay history; collections
-- (Customer, In) and supplier payments (Supplier, Out) still choose their pair
-- from the method and remain guarded.
--
-- The residual hole is the one migration 44 accepted and named for the legacy
-- refund tables: a direct SQL insert in a replaying pair can name any account.
-- Refund writers copy a pair from a parent payment guarded at birth. Resolving
-- the account from the method's current owner would silently change which box
-- funds a refund, so a guard that aborts the valid replay is not a hardening.
-- Exempting by (party_type, direction) is the honest boundary: it is exactly
-- the replaying set used by `delivery_entry_kind`, not a blanket hole.
--
-- Deliberate asymmetry: INSERT-only, no BEFORE UPDATE twin. Equality is
-- enforced when a chosen pair is born; afterwards the stored account is the
-- historical fact of where the money landed, while the method is mutable
-- configuration. Re-pointing a method must not move history.
--
-- `COALESCE(…, -1)` is load-bearing. If a method exists with a NULL account,
-- the subquery and `<>` would otherwise be NULL and WHEN would not abort; the
-- sentinel refuses that unassigned method. It also refuses a nonexistent
-- method even when foreign keys are disabled.

DROP TRIGGER IF EXISTS trg_payments_method_account_insert;

CREATE TRIGGER trg_payments_method_account_insert
BEFORE INSERT ON payments
FOR EACH ROW
WHEN NOT (
        (NEW.party_type = 'Customer' AND NEW.direction = 'Out')
        OR (NEW.party_type = 'Supplier' AND NEW.direction = 'In')
     )
     AND NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;
