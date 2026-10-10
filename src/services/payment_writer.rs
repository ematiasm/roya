//! The ONE writer of a delivery of money (P3, decisions 5 and 8).
//!
//! Lives here rather than on `SalesService` because four services need it — sales,
//! purchases, and the two return families — and a second copy would be a second
//! opinion about what a delivery looks like. That is the disease this whole family of
//! work exists to cure: five folds computing one concept.
//!
//! It is a FREE function over exactly the repositories it touches, not a method on a
//! trait, because the services that call it share no trait: they are four different
//! structs with four different sets of generics. Naming the four collaborators in the
//! signature is what lets each of them pass its own instances and keeps the writer
//! unable to touch anything else.

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

use crate::error::AppResult;

/// Selects the party-ledger entry for a cash delivery. The kind records the
/// movement in the party's obligation, not the direction cash moved:
///
/// | party, direction | what happened | kind |
/// |---|---|---|
/// | Customer, In | customer pays the shop; obligation down | `Payment` |
/// | Customer, Out | shop refunds customer; obligation up | `Refund` |
/// | Supplier, Out | shop pays supplier; obligation down | `Payment` |
/// | Supplier, In | supplier refunds shop; obligation up | `Refund` |
///
/// Thus `Refund` applies exactly when cash flow reverses that party's normal
/// flow: Customer/Out or Supplier/In. All other combinations are `Payment`.
fn delivery_entry_kind(
    party_type: crate::models::PartyType,
    direction: crate::models::PaymentDirection,
) -> crate::models::PartyEntryKind {
    use crate::models::{PartyEntryKind, PartyType, PaymentDirection};

    match (party_type, direction) {
        (PartyType::Customer, PaymentDirection::Out)
        | (PartyType::Supplier, PaymentDirection::In) => PartyEntryKind::Refund,
        (PartyType::Customer, PaymentDirection::In)
        | (PartyType::Supplier, PaymentDirection::Out) => PartyEntryKind::Payment,
    }
}

/// **THE ONE WRITER of a delivery of money** (P3, decisions 5 and 8).
///
/// Records a `payments` document, the ONE cash movement it produced, its
/// allocations, and the party-ledger entry — every write on the caller's
/// connection. The delivery's number comes from `doc_sequences` inside that
/// connection, so a rollback returns it instead of burning it.
///
/// There is deliberately ONE such method rather than one per entry point. The
/// three ways money arrives as `In` — a direct payment on a sale, a collection
/// across several invoices, and the cash tender of a `confirm` — differ only in
/// how many allocations they carry and whether a receipt groups them. Two
/// writers would be two opinions about what a delivery looks like, which is the
/// disease the payments family exists to cure.
///
/// `allocations` are `(document kind, document id, amount)` and each is applied
/// as an explicit share; the repository's cap refuses a set that exceeds
/// `amount`, and refuses it with the figures named.
pub async fn record_delivery_in<DR, A, T, PL, PY>(
    sequences: &DR,
    transactions: &crate::services::TransactionService<A, T>,
    party_ledger: &PL,
    payments: &PY,
    tx: &mut sqlx::SqliteConnection,
    actor: i64,
    direction: crate::models::PaymentDirection,
    party_type: crate::models::PartyType,
    party_id: i64,
    document: (crate::models::PartyDocumentKind, i64),
    method_id: i64,
    account_id: i64,
    amount: Decimal,
    date: NaiveDate,
    notes: Option<String>,
    // `movement_description` is the human label for the movement
    // (`description`). The reference is always the delivery's number; the
    // description is whatever a person reading a statement should see, which only
    // the caller knows.
    movement_description: Option<String>,
    // The receipt this delivery is grouped under, when a lump-sum collection
    // produced it. `None` for a direct payment on one sale.
    receipt_id: Option<i64>,
    allocations: &[(crate::models::PartyDocumentKind, i64, Decimal)],
) -> AppResult<crate::models::Payment>
where
    DR: crate::repositories::DocSequenceRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
    PL: crate::repositories::PartyLedgerRepository,
    PY: crate::repositories::PaymentRepository,
{
    // The no-gap number, inside the caller's unit.
    let year = date.year();
    let seq = sequences.next_number_in(tx, "PAYMENT", year).await?;
    let payment_number = crate::models::format_payment_number(year, seq);

    // The cash movement: ONE per delivery, stamped with the delivery's number.
    // `description` carries the party for a human reading a statement; the
    // reference is the document the money is traceable to.
    let movement = transactions
        .create_with_reference_in(
            tx,
            actor,
            account_id,
            match direction {
                crate::models::PaymentDirection::In => crate::models::TransactionKind::Income,
                crate::models::PaymentDirection::Out => crate::models::TransactionKind::Expense,
            },
            amount,
            movement_description,
            Some(payment_number.clone()),
            date,
        )
        .await?;

    let payment = payments
        .create_in(
            tx,
            &crate::models::NewPayment {
                number: payment_number.clone(),
                direction,
                party_type,
                party_id,
                method_id,
                account_id,
                amount,
                date,
                notes,
                transaction_id: Some(movement.id),
                receipt_id,
                created_by: actor,
            },
        )
        .await?;

    for (kind, target_id, share) in allocations {
        payments
            .allocate_in(
                tx,
                &crate::models::NewPaymentAllocation {
                    payment_id: payment.id,
                    target_kind: *kind,
                    target_id: *target_id,
                    amount: *share,
                    created_by: actor,
                },
            )
            .await?;
    }

    // One ledger entry per payment DOCUMENT (decision 6), not per allocation.
    let entry_kind = delivery_entry_kind(party_type, direction);
    party_ledger
        .insert_in(
            tx,
            &crate::models::NewPartyLedgerEntry {
                party_type,
                party_id,
                kind: entry_kind,
                amount: entry_kind.signed_amount(amount),
                // The caller supplies the locator explicitly. A `Payment`/`Refund`
                // entry may be multiple per document (migration 43's partial index
                // excludes those kinds for exactly that reason), so this is a locator,
                // not an identity.
                document_kind: document.0,
                document_id: document.1,
                entry_date: date,
                reference: Some(payment_number.clone()),
                created_by: actor,
            },
        )
        .await?;

    Ok(payment)
}
