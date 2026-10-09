use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Domain enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum TransactionKind {
    Income,
    Expense,
}

impl std::fmt::Display for TransactionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Income => write!(f, "Income"),
            Self::Expense => write!(f, "Expense"),
        }
    }
}

impl std::str::FromStr for TransactionKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "income" => Ok(Self::Income),
            "expense" => Ok(Self::Expense),
            _ => Err(format!("invalid transaction kind: {s}")),
        }
    }
}

// ---------------------------------------------------------------------------
// DB entities (what sqlx reads)
// ---------------------------------------------------------------------------
// T1 business configuration and tax domain.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusinessSettings {
    pub id: i64,
    pub business_name: String,
    pub default_locale_code: String,
    pub currency_code: String,
    pub timezone: String,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone)]
pub struct NewBusinessSettings {
    pub business_name: String,
    pub default_locale_code: String,
    pub currency_code: String,
    pub timezone: String,
}

#[derive(Debug, Clone)]
pub struct UpdateBusinessSettings {
    pub business_name: String,
    pub default_locale_code: String,
    pub currency_code: String,
    pub timezone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusinessLocale {
    pub id: i64,
    pub locale_code: String,
    pub language_code: String,
    pub display_name: String,
    pub is_enabled: bool,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone)]
pub struct NewBusinessLocale {
    pub locale_code: String,
    pub language_code: String,
    pub display_name: String,
    pub is_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateBusinessLocale {
    pub locale_code: String,
    pub display_name: String,
    pub is_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tax {
    pub id: i64,
    pub code: String,
    pub name: String,
    /// Decimal stored as canonical TEXT.
    pub rate: Decimal,
    pub is_active: bool,
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// The whole tax catalogue under one key, for the client that has to offer the
/// rates a line may be frozen with.
///
/// Named because the key is the contract: renaming it inside an untyped `json!`
/// compiles, passes every test in this crate, and breaks every client that
/// reads it — the failure mode this type exists to make impossible.
#[derive(Serialize)]
pub struct TaxesResponse {
    pub taxes: Vec<Tax>,
}

#[derive(Debug, Clone)]
pub struct NewTax {
    pub code: String,
    pub name: String,
    pub rate: Decimal,
    pub is_active: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateTax {
    pub code: Option<String>,
    pub name: Option<String>,
    pub rate: Option<Decimal>,
    pub is_active: Option<bool>,
}

/// What currently references one tax, split by the two families that mean
/// DIFFERENT things to the operator and are therefore never summed into a
/// single "in use" number.
///
/// The split is the whole point of the hard-delete safeguard: a product link is
/// current catalogue state the operator can undo, while a document snapshot is
/// frozen history the application will never rewrite. Each count answers a
/// different question and leads to a different remedy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaxReferenceCounts {
    /// Rows in `product_taxes`: the tax is still linked to products.
    pub product_links: i64,
    /// Rows in `sale_line_taxes` plus `purchase_line_taxes`: a document line
    /// already froze this tax's code, name, rate and contribution.
    pub document_snapshots: i64,
}

impl TaxReferenceCounts {
    /// Nothing references the tax, so it may be hard-deleted.
    pub fn is_deletable(&self) -> bool {
        self.product_links == 0 && self.document_snapshots == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductTax {
    pub id: i64,
    pub product_id: i64,
    pub tax_id: i64,
    pub created_by: i64,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProductTaxView {
    #[serde(flatten)]
    pub link: ProductTax,
    pub tax: Tax,
}

/// The taxes linked to ONE product, each row flattened with its own tax resolved.
///
/// The list is a [`ProductTaxView`] and not a bare [`ProductTax`] because the
/// link alone is not an answer: a client choosing a rate needs the code and the
/// name, and resolving it server-side is what keeps every consumer from doing
/// the same join and getting it differently. The `taxes` key is the same one the
/// product-wide catalogue publishes, which is why it is named rather than
/// inlined in a literal nobody can type-check.
#[derive(Serialize)]
pub struct ProductTaxesResponse {
    pub taxes: Vec<ProductTaxView>,
}

/// One tax's contribution to the product's tax-inclusive unit price, in the
/// shape the drawer renders: the code and name an operator recognises, the rate
/// as a percentage, and the money that rate adds to the net price.
#[derive(Debug, Clone, Serialize)]
pub struct ProductTaxBreakdownRow {
    pub code: String,
    pub name: String,
    pub rate: Decimal,
    pub amount: Decimal,
}

/// How one price field of the product drawer's edit form reads.
///
/// Empty and unreadable are DIFFERENT on purpose, because the save path treats
/// them differently and a preview that collapsed them would invent a number:
/// an empty cost is the "no cost recorded" zero the column defaults to and the
/// save accepts, while an unreadable one is a typo the save rejects and a
/// preview must refuse to guess at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceField {
    /// The form sent a number, already parsed in the request's locale.
    Value(Decimal),
    /// The form sent nothing.
    Empty,
    /// The form sent text that is not a number in this locale.
    Unreadable,
}

impl PriceField {
    /// The number this field carries, or `None` for the two states that carry
    /// no number. Never a zero: an absent value and a zero are different facts
    /// and only the caller knows which one it meant.
    pub fn as_value(self) -> Option<Decimal> {
        match self {
            Self::Value(value) => Some(value),
            Self::Empty | Self::Unreadable => None,
        }
    }
}

/// The three price fields the drawer's edit form owns, each read on its own.
/// Reading them is one shared step for the save path and for the preview, so a
/// preview can never disagree with the save it previews.
#[derive(Debug, Clone, Copy)]
pub struct PriceFields {
    pub sale_price: PriceField,
    pub cost_price: PriceField,
    pub markup_pct: PriceField,
}

/// EVERY price and cost rule that can change a figure the product price ladder
/// publishes or a save accepts. One variant per rule, never a string: the ladder
/// preview and the product form answer the same refusal for the same input, and
/// they can only do that structurally while the refusal has an identity both
/// surfaces can hold.
///
/// A variant here is a RULE, not a message. `as_str` is the English text this
/// refusal has always answered with — the body every non-localized consumer
/// (the JSON API above all) still receives, byte for byte — and the localized
/// sentence is the catalog's, reached through the one shared mapping. The two
/// are pinned together by a test, so neither can be re-worded alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceRefusal {
    /// `derive_net_sale_price`: a markup at or below -100 would derive a price
    /// that is not a price.
    MarkupNotAboveMinus100,
    /// `derive_net_sale_price`: a markup with no positive cost has nothing to
    /// derive from. "No cost" is the column's zero, never a NULL.
    MarkupNeedsPositiveCost,
    /// `derive_net_sale_price`: the operands are user-supplied and unbounded, and
    /// rust_decimal's raw arithmetic operators panic on overflow, so an
    /// unbounded pair is refused instead of crashing the handler. The same
    /// applies to every other variant here: none of them is a defect report, and
    /// each one exists so a value that cannot be carried reaches an operator as
    /// a sentence.
    DerivationOverflow,
    /// `validate_effective_prices`: a product must sell for something.
    SalePriceNotPositiveForProduct,
    /// `validate_effective_prices`: a negative price is refused for a service
    /// too, with its own sentence — the rules differ, so the identities do.
    SalePriceNegative,
    /// `validate_effective_prices`: a negative cost is refused for both kinds.
    CostPriceNegative,
    /// The form's own shape: a manual sale price the operator emptied. The save
    /// refuses it before the service is reached, and the ladder reports the same
    /// refusal rather than inventing a price.
    SalePriceRequired,
    /// `final_price::solve_final_price`: the typed tax-inclusive price is not
    /// the tax-inclusive price of ANY net. The per-contribution rounding leaves
    /// gaps, so some typed values simply do not exist as a final price.
    FinalPriceUnreachable,
    /// `final_price::solve_final_price`: the linked rates add up to -100% or
    /// less, so a net is not a function of the final price and there is no
    /// estimate to search around. The rate set itself has to change first.
    FinalPriceNotInvertible,
    /// `final_price::solve_final_price`: the solved net could not be reproduced
    /// from `cost_price` by any markup the arithmetic can REPRESENT — solving
    /// `markup = net * 100 / cost - 100` leaves the 28-digit `Decimal` range, so
    /// there is no `markup_pct` value that could ever close the round trip.
    ///
    /// This is deliberately NOT "the ladder did not close": when a rung is
    /// refused by the deriver the solve reports the DERIVER's own refusal, and
    /// `derive_net_sale_price` refusing every rung is a different fact with a
    /// different remedy. The solve says which one happened.
    FinalPriceMarkupUnreachable,
    /// `final_price::solve_final_price`: the typed final price is above
    /// `final_price::max_solvable_final_price`.
    ///
    /// The ceiling was introduced while the tax contract still multiplied with
    /// rust_decimal's raw operator and PANICKED on overflow, and it stays: the
    /// contract is now total and refuses in its own right, but a solve that
    /// already knows the price is outside what the arithmetic can carry must
    /// say so with the solve's OWN variant rather than depending on which of its
    /// two mechanisms notices first.
    FinalPriceTooLarge,
    /// `final_price::solve_final_price`: a linked rate is so large that
    /// `net * rate` leaves the representable range for a final price the solve
    /// otherwise accepts. Refused for the same reason as `FinalPriceTooLarge`,
    /// and separately named because the remedy differs: this one is the tax, not
    /// the price. Distinct from `TaxArithmeticTooLarge`, which the shared
    /// contract raises on a document line and which names the same arithmetic
    /// with the line's own vocabulary.
    TaxRateTooLargeToPrice,
    /// `final_price::solve_final_price`: the NET this final price would need is
    /// above `Decimal::MAX`, so no net price can be stored for it. The typed
    /// final price is INSIDE the solve's own limit — the linked rates gross it
    /// down by a factor small enough that dividing lands past the ceiling (100
    /// taxes at `-0.99999999999` gross a price down by 1e-11).
    ///
    /// This is deliberately neither `FinalPriceTooLarge` nor
    /// `TaxRateTooLargeToPrice`. The price is not over the limit, so the first
    /// would contradict the limit's own promise; and the individual rates are
    /// small, so the second would point at the wrong field. A third variant is
    /// the honest answer because the remedy is a third thing: the rate SET has to
    /// stop grossing the price down that far.
    NetPriceTooLarge,
    /// The purchase boundary — the place a tax-inclusive cost is converted, where
    /// a supplier's gross cost becomes a net one: the cost the operator typed is
    /// not the gross of ANY net cost. The per-contribution rounding leaves gaps
    /// for a cost exactly as it does for a final price, so some typed costs do
    /// not exist as the cost of a net.
    ///
    /// This is deliberately NOT `FinalPriceUnreachable`. The staircase gap is one
    /// fact and the arithmetic is the same, but a refusal names the figure that
    /// is actually wrong: reusing the sale sentence would tell an operator who
    /// typed a cost that their FINAL PRICE is wrong, and send them to correct a
    /// number the sale solve never read.
    CostUnreachable,
    /// The purchase boundary: the linked rates add up to -100% or less, so a net
    /// cost is not a function of the cost the operator typed and there is no
    /// estimate to search around.
    ///
    /// This is deliberately NOT `FinalPriceNotInvertible`, and the RATE SET is the
    /// reason: the rates are the thing that has to change, before anything else
    /// about this cost can be judged. The sale sentence would still have to be
    /// read past a final price to reach that, and it names the wrong figure on the
    /// way.
    CostNotInvertible,
    /// The purchase boundary: the NET cost this cost would need is above
    /// `Decimal::MAX`, so no net cost can be stored for it. The typed cost is
    /// INSIDE the conversion's own ceiling — the linked rates gross it down by a
    /// factor small enough that dividing lands past the ceiling — and every
    /// individual rate is small.
    ///
    /// This is deliberately NOT "the cost is too large", for the reason the sale
    /// twin carries: the cost is not over the limit, so a cost-too-large sentence
    /// would contradict the limit's own promise. It is deliberately not
    /// `TaxRateTooLargeToCost` either, because the individual rates are small and
    /// that sentence would point at the wrong field. The remedy is a third thing
    /// — the rate SET has to stop grossing this cost down that far — so a third
    /// sentence is the honest answer.
    CostNetTooLarge,
    /// The purchase boundary: ONE linked rate is so large that pricing this cost
    /// leaves the representable range where the conversion would otherwise
    /// accept it.
    ///
    /// This is deliberately NOT `TaxRateTooLargeToPrice`, which names a final
    /// price this refusal must not, and NOT `CostNetTooLarge`, which is a
    /// statement about the rate SET. The remedy here is a single rate — the tax,
    /// not the cost — so the two sentences send the operator to two different
    /// fields.
    TaxRateTooLargeToCost,
    /// A document line's net amount is `qty * price`, and BOTH operands are
    /// user-supplied with no ceiling of their own, so the PRODUCT leaves the
    /// 28-digit `Decimal` range before any tax is even read. The remedy is
    /// wholly the operator's: the quantity or the unit price has to come down.
    LineAmountTooLarge,
    /// The net amount is representable, and so is every individual tax
    /// contribution, but the PAIR is not: `net + SUM round2(net * rate / 100)`
    /// is what leaves the range.
    ///
    /// This is deliberately NOT `LineAmountTooLarge`, and the probe that found
    /// the difference is the reason. There are inputs where every individual
    /// multiply fits, the running tax total fits, and only the FINAL ADD
    /// overflows — `net` at the top of the range with a single 1% rate is one.
    /// The governing bound is therefore the pair `net * (1 + SUM rate/100) <=
    /// MAX`, not the per-multiply `net * rate <= MAX`, and a guard that checked
    /// only the multiplies would still crash on exactly those inputs.
    ///
    /// The remedy differs for the same reason. Lowering the quantity or the unit
    /// price is what fixes an unrepresentable amount; here either the amount OR
    /// the rate has to come down, so an operator who only lowers the price of a
    /// 1%-rate line is refused again for the same arithmetic.
    TaxArithmeticTooLarge,
    /// Every line of a document is representable, and every one of them was
    /// stored by a write that refuses to store an unrepresentable amount — and
    /// their SUM is not representable. Two lines of `4e28` with no tax are the
    /// smallest construction: each is individually carryable, and `8e28` is
    /// above `Decimal::MAX`.
    ///
    /// This is a THIRD rule and not a reuse of either line rule, for the reason
    /// the other two are separate: the remedy is a different operator action
    /// again. A `LineAmountTooLarge` says "lower this line's quantity or price";
    /// a `TaxArithmeticTooLarge` says "this line's amount or its rate". Neither
    /// is true here — every line is fine, and every rate is fine — so reusing
    /// either would tell the operator to fix a number that is already correct.
    /// What is wrong is the DOCUMENT: the operator has to reduce it, split it,
    /// or have the amounts corrected at the source.
    ///
    /// It is also the one rule a per-line bound can never reach, which is why it
    /// exists rather than a wider line rule. Per-line carryability is a
    /// statement about one row; this is a statement about a SET of rows, and a
    /// census that reads one row at a time cannot see it. The same identity
    /// covers the sums built FROM documents — a customer's outstanding balance,
    /// a debt figure — for the same reason: each document's figure is
    /// representable and the set of them is not, so the remedy is again on the
    /// documents rather than on any one line.
    DocumentTotalTooLarge,
    /// A sum over stored amounts that is not over ONE document's lines: an
    /// account's balance (its transactions) and a product's stock level (its
    /// movements). Every row in those sets is a single bounded write, and the sum
    /// of bounded rows is not bounded — the same argument as the document total,
    /// one level out, which is why it is a rule of its own rather than a reuse of
    /// that sentence: an account balance is not a document, and telling an
    /// operator to split their bank account would be nonsense.
    AggregateTooLarge,
}

impl PriceRefusal {
    /// Every rule, in the order the helpers apply them.
    ///
    /// It exists for the tests that must be TOTAL over the enum: the
    /// catalog-translation test walks it, so a new variant cannot ship without a
    /// key in BOTH catalogs and an English row byte-identical to `as_str`. A
    /// production build has no reader — which is why the attribute below is here
    /// rather than a `use` nobody would find: this is a list FOR the totality
    /// test, exactly as `MessageKey::ALL` is, and this crate is a binary, where
    /// `pub` does not by itself exempt an item from the dead-code pass.
    #[allow(dead_code)]
    pub const ALL: &'static [Self] = &[
        Self::MarkupNotAboveMinus100,
        Self::MarkupNeedsPositiveCost,
        Self::DerivationOverflow,
        Self::SalePriceNotPositiveForProduct,
        Self::SalePriceNegative,
        Self::CostPriceNegative,
        Self::SalePriceRequired,
        Self::FinalPriceUnreachable,
        Self::FinalPriceNotInvertible,
        Self::FinalPriceMarkupUnreachable,
        Self::FinalPriceTooLarge,
        Self::TaxRateTooLargeToPrice,
        Self::NetPriceTooLarge,
        Self::CostUnreachable,
        Self::CostNotInvertible,
        Self::CostNetTooLarge,
        Self::TaxRateTooLargeToCost,
        Self::LineAmountTooLarge,
        Self::TaxArithmeticTooLarge,
        Self::DocumentTotalTooLarge,
        Self::AggregateTooLarge,
    ];

    /// The English text this refusal has always answered with. It is the
    /// `AppError` body, so it is not a presentation choice: a non-localized
    /// consumer reads exactly these bytes, and the closed-catalog test pins the
    /// English translation to the same string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MarkupNotAboveMinus100 => "markup_pct must be > -100",
            Self::MarkupNeedsPositiveCost => "cost_price must be > 0 when markup_pct is set",
            Self::DerivationOverflow => {
                "markup_pct or cost_price is too large to derive a sale_price"
            }
            Self::SalePriceNotPositiveForProduct => "sale_price must be > 0 for products",
            Self::SalePriceNegative => "sale_price cannot be negative",
            Self::CostPriceNegative => "cost_price cannot be negative",
            Self::SalePriceRequired => "sale_price is required",
            Self::FinalPriceUnreachable => {
                "no net price produces this final price with the linked taxes"
            }
            Self::FinalPriceNotInvertible => {
                "linked tax rates must add up to more than -100 to solve a final price"
            }
            Self::FinalPriceMarkupUnreachable => {
                "no markup_pct derives this net_price from this cost_price"
            }
            Self::FinalPriceTooLarge => "final_price is too large to solve a net_price from",
            Self::TaxRateTooLargeToPrice => {
                "a linked tax rate is too large to price this final_price"
            }
            Self::NetPriceTooLarge => {
                "the linked tax rates gross this final_price down to a net_price that is too \
                 large to store"
            }
            Self::CostUnreachable => "no net cost grosses to this cost with the linked taxes",
            Self::CostNotInvertible => {
                "linked tax rates must add up to more than -100 to solve a cost"
            }
            Self::CostNetTooLarge => {
                "the linked tax rates gross this cost down to a net_cost that is too large to \
                 store"
            }
            Self::TaxRateTooLargeToCost => "a linked tax rate is too large to price this cost",
            // No trailing period, like every other row here: this text IS the
            // body the JSON API answers with, and the closed-catalog test pins
            // the English row to these bytes.
            Self::LineAmountTooLarge => "qty * price is too large to store on this line",
            Self::TaxArithmeticTooLarge => "the line amount is too large to calculate its taxes",
            Self::DocumentTotalTooLarge => {
                "the document total is too large to compute: reduce or split the document, or \
                 correct its amounts at the source"
            }
            Self::AggregateTooLarge => {
                "the accumulated amount is too large to compute: correct the stored amounts at \
                 the source"
            }
        }
    }
}

impl std::fmt::Display for PriceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for PriceRefusal {
    /// Serialized as the sentence, not as a variant name: a refusal that reaches
    /// a JSON body must look exactly as it did when it was a `String`.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// What the ladder is asked to price. The form values arrive already gated by
/// the caller's own form-shape rules, so this module never re-decides what an
/// empty field means — it only derives and totals.
#[derive(Debug, Clone)]
pub enum LadderInput {
    /// The drawer's current form values: the manual sale price, the cost and
    /// the markup (`None` for a manual-price product). These are the exact
    /// values a save would submit.
    ///
    /// `kind` is the kind the FORM carries, which is `None` when the request
    /// sent none or sent one that is not a kind. The price rule branches on the
    /// kind — a product must sell for something, a service may cost nothing —
    /// so binding the STORED kind here would let a free service, switched to a
    /// product in the form, publish a 0.00 the save would refuse. `None` means
    /// "no readable kind in the form", and the ladder then uses the product's
    /// own stored kind.
    Form {
        kind: Option<ProductKind>,
        sale_price: Decimal,
        cost_price: Decimal,
        markup_pct: Option<Decimal>,
    },
    /// No form values exist yet: the drawer's first render. The stored row is
    /// the answer and nothing is wrong.
    Stored,
    /// A form field was not a number in this locale. The ladder reports the
    /// product's last saved state instead of guessing at a price, and says so.
    ///
    /// This takes PRECEDENCE over every form-shape gate, which is a decision of
    /// the ladder and not of the save path: the save path checks the sale price
    /// before the cost, so a request with an unreadable cost and an empty
    /// manual price is answered for the missing price and never mentions the
    /// field the operator is typing into. A preview has nothing to say once a
    /// field is not a number, so the ladder names that instead.
    Unreadable,
    /// A form-shape refusal the SAVE would answer too — a manual price that is
    /// empty, say — so the ladder reports the same refusal rather than being
    /// the only surface that invents a price. It is a `PriceRefusal`, not a
    /// message, so the ladder and the save can only ever render it through the
    /// one shared mapping. It keeps the cost, which the form does hold, so the
    /// operator can still see the rung being edited.
    Refused {
        refusal: PriceRefusal,
        cost_price: Decimal,
    },
}

/// Derived, never stored: the product price ladder, in the order an operator
/// reads it — cost, markup, net sale price, each tax with its amount, total
/// taxes, tax-inclusive price.
///
/// Everything here is a preview of what saving the form would do, computed by
/// the same two contracts the save and a document line use
/// (`inventory::derive_net_sale_price` and `line_taxes::calculate_line_taxes`).
/// The two refusal states are part of the answer rather than an error, because
/// the ladder's job is to tell the operator what a save would do BEFORE they
/// save it: `Some` means there is no net price to show and no tax money may be
/// derived from one.
///
/// It also carries ONE figure and ONE refusal about the COST — `cost_total` and
/// `cost_refusal` — which are the cost's own answer and are NOT the same fact as
/// the net's. The four figures an operator reads are the cost, the cost with
/// taxes, the net sale price and the sale price with taxes, and the second one
/// used to be missing from a ladder whose whole job is to show all four.
#[derive(Debug, Clone, Serialize)]
pub struct ProductPriceLadder {
    /// The cost the ladder reports: the form's value, or the stored cost when
    /// the form could not be read at all.
    pub cost_price: Decimal,
    /// The cost's tax-inclusive figure: `calculate_line_taxes(cost_price,
    /// &taxes).total`, computed by the SAME contract the net's figure is, over
    /// the SAME product-scoped tax set, so the two halves of this ladder can
    /// never disagree about which taxes apply or be a cent apart on one rate.
    ///
    /// Canonical decimal, ungrouped and unlocalized: the money formatting
    /// belongs to the presentation layer, exactly as it does for every other
    /// figure on this ladder.
    ///
    /// It is a `Decimal` beside `cost_refusal` rather than something that can
    /// hold "no amount", because every sibling figure here is a `Decimal`
    /// rendered through `format_currency`, and a figure that cannot be rendered
    /// like the other four is a figure that will be rendered wrong. The cost of
    /// that choice is that the amount alone cannot express a refusal — it reads
    /// `ZERO` either way — so the presentation layer MUST guard on `cost_refusal`
    /// before printing it, exactly as it already does for `net_refusal`.
    pub cost_total: Decimal,
    /// The COST half's own refusal — a DIFFERENT FACT from `net_refusal`, in its
    /// own slot precisely because the two fail independently.
    ///
    /// `validate_effective_prices` never compares `cost_price` to the sale price,
    /// so a manual-price product may legitimately cost more than it sells for and
    /// the save accepts it. The cost is then the LARGER of the two bases, so it
    /// is the cost's arithmetic that can refuse while the net's succeeds. And a
    /// markup that fails derivation leaves a product with no net price and a
    /// perfectly storable cost. One shared slot would mean either the cost's
    /// figure is suppressed by a refusal about a different number, or the net's
    /// is, and both would be lies. Both may be `Some` at the same time.
    ///
    /// Typed, never a message, for the reason `net_refusal` is: the ladder
    /// renders refusals through the one shared mapping the save form uses, so a
    /// preview cannot say in one language what the save says in another.
    pub cost_refusal: Option<PriceRefusal>,
    /// The markup the ladder reports, or `None` for a manual-price product.
    pub markup_pct: Option<Decimal>,
    /// The net sale price the taxes are applied to. Meaningless when
    /// `net_refusal` is `Some` — which includes a tax-arithmetic refusal, where
    /// the net itself is perfectly storable and is simply not shown beside a
    /// breakdown that does not exist. The form field above the ladder still
    /// shows the stored value, so nothing is hidden from the operator.
    pub net_price: Decimal,
    /// True when `net_price` came out of the cost and the markup; false for a
    /// manual price. The ladder states which, because "why is it this number"
    /// is the first question an operator asks of a price.
    pub net_is_derived: bool,
    /// The ladder's own refusal, when no money can be stated. Typed, not a
    /// message: the ladder renders it through the same mapping the save form
    /// renders it through, so the preview cannot say in one language what the
    /// save says in another.
    ///
    /// It carries BOTH halves, because both mean the same thing to a reader —
    /// there is no figure here to show. The PRICE half is the save path's own
    /// refusal, when no net price exists at all. The TAX half is
    /// `line_taxes::calculate_line_taxes` refusing the arithmetic on a net that
    /// does exist: a stored price with no upper ceiling plus a stored rate can
    /// be a pair no arithmetic can carry, and a ladder that published a
    /// breakdown for it would be publishing a figure a document line would
    /// refuse to write.
    ///
    /// ONE slot, not two. A second field would need a second message key and a
    /// second branch in the fragment, and a reader would still be asking the
    /// same question of both.
    pub net_refusal: Option<PriceRefusal>,
    /// True when a form field was not a number, so the ladder reports the last
    /// state the save path accepted instead of a preview of a value that does
    /// not exist.
    pub inputs_unreadable: bool,
    /// PROVENANCE, and the reason it is on the model rather than in the
    /// template: `cost_price`, `markup_pct` and `net_price` are columns of the
    /// product, but on a preview they are the FORM's values, which a save would
    /// store and nothing has stored yet. Rendering a value the operator has
    /// typed as "stored" is a lie the ladder's own legend then contradicts, so
    /// the distinction is a fact about the answer, not about the markup.
    pub from_form: bool,
    /// One row per active linked tax, in the calculation contract's order.
    /// Always empty when `net_refusal` is `Some`.
    pub breakdown: Vec<ProductTaxBreakdownRow>,
    /// The sum of `breakdown`, at two decimals.
    pub tax_total: Decimal,
    /// The tax-inclusive unit price: `round(net_price + tax_total)`.
    pub total: Decimal,
}

// ---------------------------------------------------------------------------
// Line tax snapshots (tax calculation and settings)
// ---------------------------------------------------------------------------

/// One tax's immutable facts as a document line recorded them, together with
/// the contribution that tax made to the line's net subtotal.
///
/// This is the WRITE shape of a snapshot: it carries no id of its own and no
/// parent, so the same value is inserted into the sale-line or the
/// purchase-line snapshot table and the calculation contract can produce it
/// without knowing which family will consume it. `rate` and `amount` are
/// canonical decimals stored as TEXT; `tax_code`/`tax_name` are frozen copies,
/// never a live join to `taxes`. The columns are named `tax_code`/`tax_name`
/// because inside a line-tax table the qualifier is what makes the row
/// readable; the Rust fields keep the shorter shape `Tax` already uses.
#[derive(Debug, Clone)]
pub struct NewLineTax {
    pub tax_id: i64,
    pub code: String,
    pub name: String,
    /// The rate the calculation used, as a percentage.
    pub rate: Decimal,
    /// The rounded contribution of this tax to the line's net subtotal.
    pub amount: Decimal,
}

/// A stored sale-line tax snapshot: [`NewLineTax`] plus the row's identity and
/// the line it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaleLineTax {
    pub id: i64,
    pub sale_line_id: i64,
    pub tax_id: i64,
    pub code: String,
    pub name: String,
    pub rate: Decimal,
    pub amount: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

/// A stored purchase-line tax snapshot: [`NewLineTax`] plus the row's identity
/// and the line it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseLineTax {
    pub id: i64,
    pub purchase_line_id: i64,
    pub tax_id: i64,
    pub code: String,
    pub name: String,
    pub rate: Decimal,
    pub amount: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Account {
    pub id: i64,
    pub name: String,
    /// Stored as TEXT in SQLite, mapped via rust_decimal db-sqlx feature
    pub cached_balance: Decimal,
    /// Audit actor (M5 Phase B): the user id that created the row. The
    /// interface resolves it to a display name; it never shows the id.
    pub created_by: i64,
    /// The user id of the last edit, when the row has been edited at all.
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Transaction {
    pub id: i64,
    pub account_id: i64,
    pub kind: TransactionKind,
    pub amount: Decimal,
    pub description: String,
    /// Opaque source reference (document number). NULL for manual transactions.
    pub reference: Option<String>,
    pub date: NaiveDate,
    /// Audit actor (M5 Phase B): who created the movement, and who last edited
    /// it. A movement produced inside a document flow carries the acting user
    /// of the flow's request, never a fresh actor. The interface resolves these
    /// ids to display names; it never shows the ids.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

impl Transaction {
    pub fn is_income(&self) -> bool {
        self.kind == TransactionKind::Income
    }
    pub fn is_expense(&self) -> bool {
        self.kind == TransactionKind::Expense
    }
}

/// The transaction list the finance JSON API returns, already narrowed by the
/// request's [`TransactionFilter`].
///
/// The filter lives in the query string and the rows live under one key here;
/// naming that key is what stops a rename from reaching production behind a
/// green build, because a `json!` literal cannot fail to compile.
#[derive(Serialize)]
pub struct TransactionsResponse {
    pub transactions: Vec<Transaction>,
}

// ---------------------------------------------------------------------------
// DTOs / API payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    /// Methods to tick for the new account, the same way the web form does.
    ///
    /// Absent means "just the defaults this account's NAME implies" (`Caja` gets
    /// `Cash`), which is what a caller that only wants an account should get.
    /// Present means the operator named them: an id owned by another account is
    /// DUPLICATED into this one, never stolen, and a method the name already
    /// brought is simply kept — the tick and the default are the same intent.
    #[serde(default)]
    pub method_ids: Vec<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTransactionRequest {
    pub account_id: i64,
    #[serde(rename = "type")]
    pub kind: TransactionKind,
    pub amount: Decimal,
    pub description: Option<String>,
    /// Optional opaque source reference; manual transactions omit it.
    pub reference: Option<String>,
    pub date: NaiveDate,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTransactionRequest {
    #[serde(rename = "type")]
    pub kind: Option<TransactionKind>,
    pub amount: Option<Decimal>,
    pub description: Option<String>,
    pub date: Option<NaiveDate>,
}

#[derive(Debug, Deserialize)]
pub struct TransactionFilter {
    pub account_id: Option<i64>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

// API responses --------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct AccountWithBalance {
    pub id: i64,
    pub name: String,
    /// The balance derived from this account's transactions, or the rule that
    /// stopped the sum. A refused balance keeps the row's place in every list
    /// that renders it, which is the same decision the document lists took: one
    /// account an operator must fix must not empty the finance page around it.
    pub balance: SetMoney,
    /// The cached balance, and `None` exactly when [`Self::balance`] refused.
    ///
    /// It is a CACHE of a sum that cannot be made, so publishing it beside a
    /// refusal would be publishing a stale figure in the place of the figure —
    /// worse than publishing none, because the operator cannot tell the two apart.
    /// Omitted from the wire entirely when refused, which keeps an ordinary
    /// account's JSON byte-identical to what it always was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_balance: Option<Decimal>,
    /// Audit actor (M5 Phase B): who created and who last edited the account.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
}

/// The account list the finance JSON API returns: the rows, plus the balance of
/// ALL of them under one key.
///
/// The envelope is named because these two keys are a wire contract, and a
/// `json!` literal is not a contract the compiler holds: rename the key and
/// every test still passes while every consumer reading it silently reads
/// `null`. Naming the object is what makes that rename a compile error instead.
///
/// `total_balance` is a [`SetMoney`], exactly like each row's `balance`, because
/// it is a set sum over the same accounts — and it therefore keeps the three
/// wire states it has always had: the decimal string when the sum carried, the
/// rule when it refused, so a client can read a headline that says why it is
/// not a number rather than one it cannot account for.
#[derive(Serialize)]
pub struct AccountsResponse {
    pub accounts: Vec<AccountWithBalance>,
    pub total_balance: SetMoney,
}

#[derive(Debug, Serialize)]
pub struct AccountDetail {
    pub id: i64,
    pub name: String,
    /// The derived balance, or the rule that stopped the sum — the same shape the
    /// list row carries, because a detail page is a list of one.
    pub balance: SetMoney,
    /// Audit actor (M5 Phase B): who created and who last edited the account.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub transactions: Vec<Transaction>,
}

#[derive(Debug, Serialize)]
pub struct DashboardData {
    pub total_balance: Decimal,
    pub accounts: Vec<AccountWithBalance>,
}

// ---------------------------------------------------------------------------
// M1 inventory domain (DB entities + inputs + views, English names)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum ProductKind {
    Product,
    Service,
}

impl std::fmt::Display for ProductKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Product => write!(f, "Product"),
            Self::Service => write!(f, "Service"),
        }
    }
}

impl std::str::FromStr for ProductKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "product" => Ok(Self::Product),
            "service" => Ok(Self::Service),
            _ => Err(format!("invalid product kind: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum MovementType {
    In,
    Out,
    Adjust,
}

impl std::fmt::Display for MovementType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::In => write!(f, "In"),
            Self::Out => write!(f, "Out"),
            Self::Adjust => write!(f, "Adjust"),
        }
    }
}

impl std::str::FromStr for MovementType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "in" => Ok(Self::In),
            "out" => Ok(Self::Out),
            "adjust" => Ok(Self::Adjust),
            _ => Err(format!("invalid movement type: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum MovementReason {
    Purchase,
    Sale,
    #[serde(
        rename = "Sale-return",
        alias = "SaleReturn",
        alias = "sale_return",
        alias = "salereturn"
    )]
    #[sqlx(rename = "Sale-return")]
    SaleReturn,
    #[serde(
        rename = "Purchase-return",
        alias = "PurchaseReturn",
        alias = "purchase_return",
        alias = "purchasereturn"
    )]
    #[sqlx(rename = "Purchase-return")]
    PurchaseReturn,
    Loss,
    Adjust,
    Initial,
}

impl std::fmt::Display for MovementReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Purchase => write!(f, "Purchase"),
            Self::Sale => write!(f, "Sale"),
            Self::SaleReturn => write!(f, "Sale-return"),
            Self::PurchaseReturn => write!(f, "Purchase-return"),
            Self::Loss => write!(f, "Loss"),
            Self::Adjust => write!(f, "Adjust"),
            Self::Initial => write!(f, "Initial"),
        }
    }
}

impl std::str::FromStr for MovementReason {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "purchase" => Ok(Self::Purchase),
            "sale" => Ok(Self::Sale),
            "sale-return" | "sale_return" | "salereturn" | "sale return" => Ok(Self::SaleReturn),
            "purchase-return" | "purchase_return" | "purchasereturn" | "purchase return" => {
                Ok(Self::PurchaseReturn)
            }
            "loss" => Ok(Self::Loss),
            "adjust" => Ok(Self::Adjust),
            "initial" => Ok(Self::Initial),
            _ => Err(format!("invalid movement reason: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    /// Audit actor: the acting user's id (M5 Phase B, slice S10) — the row's
    /// creator, and its last editor when one exists.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// The category tree flat, under one key, for a client building its own select.
///
/// Named because the key is the contract. A `json!` literal is checked by
/// nothing: renaming `categories` compiles, every test here still reads the
/// literal it wrote itself, and the only thing that notices is the client.
#[derive(Serialize)]
pub struct CategoriesResponse {
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    pub id: i64,
    pub sku: String,
    pub name: String,
    pub kind: ProductKind,
    pub category_id: Option<i64>,
    pub unit: String,
    pub sale_price: Decimal,
    pub cost_price: Decimal,
    /// Percentage over `cost_price` used to derive `sale_price` (pricing T1).
    /// `None` means "no markup, manual price" — a real value, not an absence;
    /// the derivation itself lands in a later slice (T4).
    pub markup_pct: Option<Decimal>,
    pub track_stock: bool,
    pub min_stock: Option<Decimal>,
    pub max_stock: Option<Decimal>,
    pub location: Option<String>,
    pub notes: Option<String>,
    pub is_active: bool,
    /// Audit actor: the acting user's id (M5 Phase B, slice S10).
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

impl Product {
    /// Template helper for the drawer's category select: `true` when this product
    /// belongs to the given category. Askama binds `Some`/match arms by reference
    /// and cannot deref or build `Some(...)` in expressions, so the comparison
    /// lives here instead of in the template.
    pub fn category_is(&self, id: &i64) -> bool {
        self.category_id == Some(*id)
    }

    /// Display form of the sale price (product-markup T7). See `money_display`
    /// for the rule; a display method per field keeps every template call site
    /// a plain `{{ ... }}` instead of a function import.
    pub fn sale_price_display(&self) -> String {
        money_display(self.sale_price)
    }

    /// Display form of the cost price, same rule and never-lie reason as the
    /// sale price above.
    pub fn cost_price_display(&self) -> String {
        money_display(self.cost_price)
    }
}

/// The product catalogue, optionally narrowed to one category, under one key.
///
/// The rows are the stored [`Product`]s and NOT [`ProductStock`]s: the catalogue
/// endpoint is the one place a client reads what exists, and the derived level
/// has its own endpoint (`/api/products/{id}/stock`) because a level is a set sum
/// that can refuse while the product row never can.
#[derive(Serialize)]
pub struct ProductsResponse {
    pub products: Vec<Product>,
}

/// Human display of a money value (product-markup T7). A stored value at
/// scale 2 or below is normalised UP to exactly two decimals (`7.5` →
/// `"7.50"`), so a list never mixes `$100` and `$100.00` side by side. A
/// stored value with MORE than two decimals prints exactly as stored —
/// rounding for display would misstate the price the customer is charged
/// (the transaction surface deliberately allows finer amounts), so the
/// normalisation is guarded by the scale check and can never round.
/// Display only: nothing here changes what is stored or validated.
pub fn money_display(d: Decimal) -> String {
    let mut out = d;
    if out.scale() <= 2 {
        // `rescale` up is exact (appending zeros); the guard above means the
        // value already fits in two decimals, so no rounding can occur.
        out.rescale(2);
    }
    out.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductBarcode {
    pub id: i64,
    pub product_id: i64,
    pub code: String,
    pub created_at: chrono::NaiveDateTime,
}

/// Every barcode of ONE product, under one key — the read side of the
/// `add_barcode` that writes them.
///
/// A scanner client resolves code → product through this list, so the key is a
/// lookup contract and not a display detail. That is exactly the class of key a
/// `json!` literal lets you rename with a green build and a silent break.
#[derive(Serialize)]
pub struct BarcodesResponse {
    pub barcodes: Vec<ProductBarcode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StockMovement {
    pub id: i64,
    pub product_id: i64,
    /// Stored magnitude as TEXT; for `Adjust` it may already be signed.
    pub qty: Decimal,
    pub movement_type: MovementType,
    pub reason: MovementReason,
    pub reference: String,
    pub date: NaiveDate,
    /// Audit actor: the acting user's id (M5 Phase B, slice S10) — for a
    /// movement produced inside a sale/purchase flow, the flow's request
    /// actor, never a fresh one (AC18).
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
}

impl StockMovement {
    /// Signed contribution of this movement to derived stock.
    pub fn signed_qty(&self) -> Decimal {
        match self.movement_type {
            MovementType::In => self.qty,
            MovementType::Out => -self.qty,
            MovementType::Adjust => self.qty,
        }
    }
}

/// The movement ledger, scoped to one product when the request names one.
///
/// Named because `movements` is the key a client pages through and audits
/// against; a literal is not a thing that can fail to compile when the contract
/// moves underneath it.
#[derive(Serialize)]
pub struct MovementsResponse {
    pub movements: Vec<StockMovement>,
}

/// Service-level input for product creation (mirrors finance request DTOs).
#[derive(Debug, Clone)]
pub struct NewProduct {
    pub sku: String,
    pub name: String,
    pub kind: ProductKind,
    pub category_id: Option<i64>,
    pub unit: String,
    pub sale_price: Decimal,
    pub cost_price: Decimal,
    pub markup_pct: Option<Decimal>,
    pub track_stock: bool,
    pub min_stock: Option<Decimal>,
    pub max_stock: Option<Decimal>,
    pub location: Option<String>,
    pub notes: Option<String>,
}

/// Service-level patch for product edits. `None` means "leave unchanged"; the web
/// form always sends every field, so it builds a full patch from the form.
/// `Option<Option<T>>` fields distinguish "leave unchanged" (`None`) from
/// "clear" (`Some(None)`), like `UpdateSupplier` / `UpdateCustomer`.
#[derive(Debug, Clone, Default)]
pub struct UpdateProduct {
    pub sku: Option<String>,
    pub name: Option<String>,
    pub kind: Option<ProductKind>,
    pub category_id: Option<Option<i64>>,
    pub unit: Option<String>,
    pub sale_price: Option<Decimal>,
    pub cost_price: Option<Decimal>,
    /// Double option on purpose, unlike the plain `Option<Decimal>` money
    /// fields above: outer `None` means "leave unchanged", `Some(None)` means
    /// "clear it back to no markup", and `Some(Some(v))` sets the markup. NULL
    /// is a real value here ("manual price"), not an absence, so the three
    /// states must be distinguishable.
    pub markup_pct: Option<Option<Decimal>>,
    pub track_stock: Option<bool>,
    pub min_stock: Option<Option<Decimal>>,
    pub max_stock: Option<Option<Decimal>>,
    pub location: Option<Option<String>>,
    pub notes: Option<Option<String>>,
}

/// Service-level input for stock movements.
#[derive(Debug, Clone)]
pub struct NewMovement {
    pub product_id: i64,
    pub qty: Decimal,
    pub movement_type: MovementType,
    pub reason: MovementReason,
    pub reference: String,
    pub date: NaiveDate,
}

/// Derived stock view (never stored as source of truth).
#[derive(Debug, Clone, Serialize)]
pub struct ProductStock {
    pub product: Product,
    /// The level derived from this product's movements, or the rule that stopped
    /// the sum. A stock level is a set sum like an account balance, so it refuses
    /// the same way, and a catalogue of products must render the one that cannot
    /// be measured instead of failing to list.
    pub stock: SetMoney,
    /// `max_stock - stock` when `stock <= min_stock`, else `None` — and `None` when
    /// the level refused, because a suggestion computed from a level that does not
    /// exist would be a number with nothing behind it.
    pub suggested: Option<Decimal>,
}

/// The reorder list: tracked products at or below their minimum level.
///
/// Two envelopes and not one parameterised type, because `low_stock` and
/// `negative_stock` are two published keys and a client that has to be told which
/// one it is reading is a client that can guess wrong. Naming both makes each a
/// contract the compiler holds.
#[derive(Serialize)]
pub struct LowStockResponse {
    pub low_stock: Vec<ProductStock>,
}

/// The same rows filtered to the levels that went BELOW zero — a distinct set,
/// not a subset flag, so it gets its own key and its own type.
#[derive(Serialize)]
pub struct NegativeStockResponse {
    pub negative_stock: Vec<ProductStock>,
}

// ---------------------------------------------------------------------------
// M2 sales domain (orchestrator, Odoo-style). Decimal-as-TEXT like finance.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum SaleStatus {
    Draft,
    Confirmed,
    Cancelled,
}

impl std::fmt::Display for SaleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "Draft"),
            Self::Confirmed => write!(f, "Confirmed"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

impl std::str::FromStr for SaleStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "confirmed" => Ok(Self::Confirmed),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid sale status: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum PaymentType {
    Cash,
    Credit,
}

impl std::fmt::Display for PaymentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cash => write!(f, "Cash"),
            Self::Credit => write!(f, "Credit"),
        }
    }
}

impl std::str::FromStr for PaymentType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "cash" => Ok(Self::Cash),
            "credit" => Ok(Self::Credit),
            _ => Err(format!("invalid payment type: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum PaymentStatus {
    Paid,
    Partial,
    Unpaid,
}

impl std::fmt::Display for PaymentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paid => write!(f, "Paid"),
            Self::Partial => write!(f, "Partial"),
            Self::Unpaid => write!(f, "Unpaid"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sale {
    pub id: i64,
    pub sale_number: Option<String>,
    pub status: SaleStatus,
    pub payment_type: PaymentType,
    /// The owning customer; the seeded walk-in for anonymous cash sales.
    pub customer_id: i64,
    /// Frozen snapshot of the customer's name at creation time, so correcting the
    /// customer never rewrites history.
    pub customer_name: String,
    pub sale_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub receipt_no: Option<String>,
    pub notes: String,
    pub cancel_reason: Option<String>,
    /// Audit actor (M5 Phase B, slice S11): who created the sale and who last
    /// edited it (a header edit, the confirm or the cancel). A line adds no
    /// columns of its own: it inherits the sale's actor.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub confirmed_at: Option<chrono::NaiveDateTime>,
    pub cancelled_at: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaleLine {
    pub id: i64,
    pub sale_id: i64,
    pub product_id: i64,
    /// Decimal qty > 0, stored as TEXT.
    pub qty: Decimal,
    /// Decimal unit_price >= 0, frozen at confirm, stored as TEXT.
    pub unit_price: Decimal,
    /// Decimal tax total stored as TEXT: the sum of this line's snapshotted
    /// tax contributions, written when the line's taxes are computed. The net
    /// subtotal and the tax-inclusive total are NOT stored — they are derived
    /// from `qty`, `unit_price` and this value, so no line can hold a total
    /// that disagrees with its own quantity and price.
    pub tax_total: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

impl SaleLine {
    pub fn subtotal(&self) -> Decimal {
        self.qty * self.unit_price
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SalePayment {
    pub id: i64,
    pub sale_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    /// Decimal amount > 0, stored as TEXT.
    pub amount: Decimal,
    pub date: NaiveDate,
    /// Finance transaction this payment created (NULL for historical rows).
    pub transaction_id: Option<i64>,
    /// Refund transaction created when the sale was cancelled, if any.
    pub refund_transaction_id: Option<i64>,
    /// Customer receipt that groups this payment, when a lump-sum collection
    /// produced it; NULL for a direct payment on a single sale.
    pub receipt_id: Option<i64>,
    /// The owning sale's document number, resolved by the receipt-allocation read
    /// so a receipt names the sale the way the user does; `None` in other reads.
    pub sale_number: Option<String>,
    /// Audit actor (M5 Phase B, slice S11): the acting user of the request that
    /// recorded the payment — for a receipt-grouped payment, the collection
    /// request's actor, never a fresh one (AC18). `updated_by` is the user who
    /// linked a refund to it, when the sale was cancelled.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

// ---------------------------------------------------------------------------
// M0 payment methods (finance-owned, account-owned 1:N). Seeded Cash/Transfer/
// Debit/CreditCard/QR, no Other. Each method belongs to at most one account
// (`account_id`, NULL = unassigned and unusable); UNIQUE(account_id, name) lets
// two accounts each own a same-named method as separate rows.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentMethod {
    pub id: i64,
    pub name: String,
    /// The owning account. Not `Option`: migration 45 made the column NOT NULL,
    /// so an unowned method is not a state this schema can hold. A method taken
    /// out of service keeps this owner and flips `is_active`.
    pub account_id: i64,
    pub is_active: bool,
    /// Audit actor (M5 Phase B): who created and who last changed the method.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// Every payment method in the catalogue, so a client can resolve a method name
/// to a row without first reading one account's catalog.
///
/// Named because `methods` is a published key: an untyped `json!` literal lets
/// it be renamed without a single compile error, which is precisely the kind of
/// change that reaches production with a green build and breaks every consumer.
#[derive(Serialize)]
pub struct MethodsResponse {
    pub methods: Vec<PaymentMethod>,
}

/// The method set of ONE account, as both GET and PUT return it.
///
/// One shape for both verbs on purpose: a client that reads a set and later
/// replaces it should not have to translate between two different answers to the
/// same question, and a second shape is a second thing to keep in step.
///
/// `method_ids` is the payload a replacement PUTs back, so it travels beside the
/// rows it names — the ids and the methods are the same fact read two ways, and
/// a client that has only one of them cannot check a replacement against what it
/// replaces.
#[derive(Serialize)]
pub struct AccountMethodsResponse {
    pub account_id: i64,
    pub method_ids: Vec<i64>,
    pub methods: Vec<PaymentMethod>,
}

/// One method with its owning account resolved for display, so method-only
/// selects render `"Name — AccountName"` without SQL in a route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentMethodWithAccount {
    pub id: i64,
    pub name: String,
    pub account_id: i64,
    pub account_name: String,
    pub is_active: bool,
}

impl PaymentMethodWithAccount {
    /// Select label: `"Transfer — Bank"`.
    ///
    /// One method, not two: `account_name` is `NOT NULL` (migration 45 made the
    /// foreign key mandatory and the read an INNER join), so a separate
    /// `account_label()` had exactly one caller and returned a clone of the field
    /// it was given. It was the site of the old `unassigned` fallback, which no
    /// row can reach any more.
    pub fn label(&self) -> String {
        format!("{} — {}", self.name, self.account_name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocSequence {
    pub doc_type: String,
    pub year: i32,
    pub last_number: i64,
}

/// Service-level input for sale creation (Draft). The service resolves the
/// customer through `CustomerService` and freezes `customer_name` from it.
#[derive(Debug, Clone)]
pub struct NewSale {
    pub customer_id: i64,
    pub payment_type: PaymentType,
    pub sale_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub receipt_no: Option<String>,
    pub notes: Option<String>,
}

/// Service-level patch for Draft header edits. The customer (and therefore the
/// name snapshot) is fixed at creation; only dates, receipt and notes are edited.
#[derive(Debug, Clone, Default)]
pub struct UpdateSaleDraft {
    pub sale_date: Option<NaiveDate>,
    pub due_date: Option<Option<NaiveDate>>,
    pub receipt_no: Option<Option<String>>,
    pub notes: Option<String>,
}

/// Aggregated sale view with derived totals (never stored as truth).
///
/// `net_subtotal` and `tax_total` are the two parts of `total`, both derived
/// from the lines: the net is `qty * unit_price` and the tax is the sum of the
/// tax totals those lines froze. They are carried separately so a detail view
/// can show an operator WHY the total is what it is, and so the parts can be
/// checked against each other. `total` is the tax-inclusive figure, and it is
/// the one `paid`/`due` and the payment ceilings are measured against.
#[derive(Debug, Clone, Serialize)]
pub struct SaleDetail {
    pub sale: Sale,
    pub lines: Vec<SaleLine>,
    pub payments: Vec<SalePayment>,
    /// `sum(line.subtotal())` — the money before tax.
    pub net_subtotal: Decimal,
    /// `sum(line.tax_total)` — the money the lines' frozen snapshots charge.
    pub tax_total: Decimal,
    /// The tax-inclusive total: the sum of each line's pinned line total.
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

impl SaleDetail {
    pub fn payment_status_for(total: Decimal, paid: Decimal) -> PaymentStatus {
        let due = total - paid;
        if due <= Decimal::ZERO {
            PaymentStatus::Paid
        } else if paid > Decimal::ZERO {
            PaymentStatus::Partial
        } else {
            PaymentStatus::Unpaid
        }
    }
}

/// The sales list the JSON API returns: a detail per document, under one key.
///
/// [`SaleDetail`] and not [`SaleListRow`], because this endpoint is a machine
/// contract: its totals are plain decimals and a document whose lines cannot be
/// added up is an error here, while the page's row type exists precisely to
/// render that case. The key is named so a rename cannot ship behind a green
/// build.
#[derive(Serialize)]
pub struct SalesResponse {
    pub sales: Vec<SaleDetail>,
}

/// Format `YYYY-SALE-NNNNNN` with zero-padded 6-digit sequence.
pub fn format_sale_number(year: i32, seq: i64) -> String {
    format!("{year}-SALE-{seq:06}")
}

/// Fold a search string to its comparable ASCII form: Unicode lowercase plus the
/// Spanish and Latin-1 diacritics mapped to their base letters. Both sides of every
/// party and catalogue match go through this one function, so `Perez` finds
/// `Pérez`, `CAFE` finds `Café` and `Ñandú` finds `ñandú`.
pub fn normalize_search(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars().flat_map(char::to_lowercase) {
        match ch {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => out.push('a'),
            'æ' => out.push_str("ae"),
            'ç' | 'ć' | 'č' => out.push('c'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' => out.push('e'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' => out.push('i'),
            'ð' => out.push('d'),
            'ñ' | 'ń' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => out.push('o'),
            'œ' => out.push_str("oe"),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'þ' => out.push_str("th"),
            'ß' => out.push_str("ss"),
            other => out.push(other),
        }
    }
    out
}

/// Server-side filter for the sales list (redesign-interface N5). Every field is
/// optional and an absent field adds no constraint, so an empty filter returns the
/// whole list and a filter matching nothing returns an empty list rather than an
/// error. `customer` is the typed party name; the service resolves it against the
/// customers table (normalized) into `customer_ids`, and the repository narrows the
/// document query by those ids. `number` matches the document number partially.
#[derive(Debug, Clone, Default)]
pub struct SaleListFilter {
    pub status: Option<SaleStatus>,
    /// The typed party name, resolved by the service into `customer_ids`.
    pub customer: Option<String>,
    /// Matching customer ids, set by the service; `Some(empty)` matches nothing.
    pub customer_ids: Option<Vec<i64>>,
    pub number: Option<String>,
    /// Inclusive lower bound on `sale_date`.
    pub from: Option<NaiveDate>,
    /// Inclusive upper bound on `sale_date`.
    pub to: Option<NaiveDate>,
}

// ---------------------------------------------------------------------------
// Sale record page (redesign-interface N2)
//
// The persisted line only carries `product_id` and the payment only carries
// `account_id`/`method_id`. These views carry the display names the record page
// shows, resolved by the service through the existing inventory and finance
// read paths, never by SQL in a route.
// ---------------------------------------------------------------------------

/// One frozen tax row as a document detail shows it: the identity and rate the
/// line recorded, plus the contribution that rate made. Never re-read from
/// `taxes`, so an edited or deactivated tax cannot rewrite what a document
/// shows. Both snapshot tables map into this one shape, so a sale line and a
/// purchase line render with the same markup.
#[derive(Debug, Clone, Serialize)]
pub struct LineTaxView {
    pub code: String,
    pub name: String,
    /// The rate the line was charged, as a percentage.
    pub rate: Decimal,
    /// The contribution to this line, already at two decimals.
    pub amount: Decimal,
}

impl From<&SaleLineTax> for LineTaxView {
    fn from(snapshot: &SaleLineTax) -> Self {
        Self {
            code: snapshot.code.clone(),
            name: snapshot.name.clone(),
            rate: snapshot.rate,
            amount: snapshot.amount,
        }
    }
}

impl From<&PurchaseLineTax> for LineTaxView {
    fn from(snapshot: &PurchaseLineTax) -> Self {
        Self {
            code: snapshot.code.clone(),
            name: snapshot.name.clone(),
            rate: snapshot.rate,
            amount: snapshot.amount,
        }
    }
}

/// One sale line resolved for `/sales/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct SaleLineView {
    pub id: i64,
    pub product_name: String,
    pub product_sku: String,
    /// The line's product id: the only key a caller needs to resolve the
    /// product's CURRENT state (active flag, stock settings) beyond the names
    /// resolved here.
    pub product_id: i64,
    pub qty: Decimal,
    pub unit_price: Decimal,
    /// The NET subtotal: `qty * unit_price`, before any tax.
    pub subtotal: Decimal,
    /// The tax this line froze when it was written. Zero for a product with no
    /// linked tax.
    pub tax_total: Decimal,
    /// The tax-inclusive line total: `round(subtotal + tax_total)`. The
    /// document total is the sum of these, so the page reconciles.
    pub total: Decimal,
    /// The frozen breakdown, empty for a product with no linked tax.
    pub taxes: Vec<LineTaxView>,
    /// The same predicate `confirm` and `cancel` use to decide whether a line
    /// moves stock (`product.kind == Product && product.track_stock`), filled
    /// from the very product read that resolves the name — so any preview
    /// built from this view cannot drift from what those flows will do.
    pub tracks_stock: bool,
}

/// One sale payment resolved for `/sales/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct SalePaymentView {
    pub id: i64,
    pub account_name: String,
    pub method_name: String,
    pub amount: Decimal,
    pub date: NaiveDate,
}

/// A document's derived money, held together because it is ONE fact: a document
/// whose lines can be added up has a net, a tax total, a tax-inclusive total, a
/// paid figure, a due balance and a payment status, and a document whose lines
/// cannot be added up has NONE of them.
///
/// The net money, the tax money and the tax-inclusive total, in that order, so
/// the page can show an auditable sum instead of one opaque number.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RecordMoney {
    pub net_subtotal: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

/// A money figure derived from a SET of documents: the amount, or the rule that
/// stopped the accumulation.
///
/// It is its own type because a partial sum must never be published. A figure
/// that silently omitted the one document whose lines cannot be added up is
/// indistinguishable from a real balance, and an operator who acts on it acts on
/// a number this application cannot stand behind. `amount` is `None` exactly
/// when `refusal` is `Some`, so "no figure" and "here is why" travel together and
/// a page has one thing to render in place of the figure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetMoney {
    pub amount: Option<Decimal>,
    pub refusal: Option<PriceRefusal>,
}

impl SetMoney {
    /// The ordinary case: a figure the accumulation could carry.
    pub fn amount(amount: Decimal) -> Self {
        Self {
            amount: Some(amount),
            refusal: None,
        }
    }

    /// The refused case: no figure at all, and the rule that stopped the sum.
    pub fn refused(refusal: PriceRefusal) -> Self {
        Self {
            amount: None,
            refusal: Some(refusal),
        }
    }
}

impl SetMoney {
    /// Whether the figure is positive — the sign a ledger row's chip reads.
    ///
    /// A refused figure is positive, because the entry that carries it is a
    /// document entering the ledger and the chip must not claim the opposite.
    /// It is a colour, never an amount: there is no figure to be right about.
    pub fn amount_is_positive(&self) -> bool {
        self.amount
            .map(|amount| amount.is_sign_positive())
            .unwrap_or(true)
    }

    /// Whether the figure is negative — the sign a money row's colour reads. A
    /// refused figure is NOT negative: it states no sign, because it states no
    /// figure.
    pub fn amount_is_negative(&self) -> bool {
        self.amount
            .map(|amount| amount.is_sign_negative())
            .unwrap_or(false)
    }
}

impl Default for SetMoney {
    /// The default is a figure, not a refusal: an absent aggregate is a set with
    /// no documents in it, and "0 owed" is a fact. `None` is reserved for the one
    /// case that is not a fact.
    fn default() -> Self {
        SetMoney::amount(Decimal::ZERO)
    }
}

impl Serialize for SetMoney {
    /// The bare amount when there is one, and an OBJECT carrying the rule when the
    /// accumulation refused.
    ///
    /// The JSON API's existing shape for these figures IS the amount, so every
    /// figure that carries stays byte-identical — a consumer that reads a normal
    /// ageing, a normal balance or a normal statement total sees exactly the
    /// bytes it saw before this type existed.
    ///
    /// A refused figure was `null` here once, and `null` was a defect: on
    /// `/api/customers/ageing` it produced four genuine-looking `0.00` cells and
    /// a `null` balance, which a client cannot tell from "this customer owes
    /// nothing". The wire now has to be able to say WHICH RULE stopped the sum,
    /// and it says it with [`PriceRefusal::as_str`] — the same bytes the
    /// `AppError` body and the English catalog row already carry, so there is no
    /// second message to keep in step.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match (self.amount, self.refusal) {
            (Some(amount), _) => serde::Serialize::serialize(&amount, serializer),
            (None, Some(refusal)) => {
                use serde::ser::SerializeStruct;
                let mut out = serializer.serialize_struct("SetMoney", 1)?;
                out.serialize_field("refused", refusal.as_str())?;
                out.end()
            }
            // Unreachable by construction: `amount` is `None` exactly when
            // `refusal` is `Some`. Serialized as `null` rather than panicking,
            // because a serializer that panics is the defect this whole type
            // exists to remove.
            (None, None) => serializer.serialize_none(),
        }
    }
}

/// One row of a document LIST: the stored document, its money when the
/// arithmetic carried it, and the rule when it did not.
///
/// This is the shape that makes a list page total. A `SaleDetail` cannot be the
/// row type, because its money is not optional: a detail is a machine contract
/// (the JSON API) where a refusal is an error, while a page must still SHOW the
/// document and say why its figure is missing. Both are built by the same
/// checked derivation, so they cannot disagree about which documents are
/// refusable.
#[derive(Debug, Clone, Serialize)]
pub struct SaleListRow {
    pub sale: Sale,
    /// The document's money, or `None` when its lines cannot be added up.
    pub money: Option<RecordMoney>,
    /// The rule that refused [`Self::money`], when it is `None`.
    pub total_refusal: Option<PriceRefusal>,
}

/// The purchase twin of [`SaleListRow`], plus the one non-money fact a purchases
/// row shows that a sale row does not: how many lines the document carries.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseListRow {
    pub purchase: Purchase,
    pub line_count: usize,
    /// The document's money, or `None` when its lines cannot be added up.
    pub money: Option<RecordMoney>,
    /// The rule that refused [`Self::money`], when it is `None`.
    pub total_refusal: Option<PriceRefusal>,
}

/// The sale record page payload: the stored document plus every child with its
/// internal keys replaced by display names. Totals stay derived.
///
/// `money` and `total_refusal` are one fact in two halves, and they are never
/// both set or both clear: the record page is the ONE surface that renders a
/// document whose total cannot be carried, because it is also the operator's
/// only way back into one. Every line is still shown with its own money — each
/// of those is representable — so the page is where the document can be read and
/// reduced, rather than a document nothing in the application can open.
#[derive(Debug, Clone, Serialize)]
pub struct SaleRecord {
    pub sale: Sale,
    pub lines: Vec<SaleLineView>,
    pub payments: Vec<SalePaymentView>,
    /// The document's money, or `None` when its lines cannot be added up.
    pub money: Option<RecordMoney>,
    /// The rule that refused [`Self::money`], when it is `None`. The page states
    /// it through the one shared `price_refusal_key` mapping, so this surface
    /// cannot invent a wording of its own.
    pub total_refusal: Option<PriceRefusal>,
}

/// The outstanding receivables endpoint's answer: one [`SaleDetail`] per
/// document the shop is still owed, under one key.
///
/// The same detail type `list_sales` returns, NOT the [`SaleRecord`] the record
/// page uses and NOT the bounded [`DebtSummary`] the banner uses — this endpoint
/// is the full receivable for a client that has to reconcile against it, and
/// each row's money is a plain total or the request failed. Naming `debt` keeps
/// the rename from reaching a consumer that a `json!` literal would never warn.
#[derive(Serialize)]
pub struct DebtResponse {
    pub debt: Vec<SaleDetail>,
}

/// The sales page's debt banner: a summary, not the full receivable. `total` and
/// `count` are exact (decimal sums in Rust) and `oldest` is the first few unpaid
/// documents by due date, so the banner renders a bounded number of rows. The full
/// receivable list stays a filtered read, never an always-rendered panel.
///
/// The total is a [`SetMoney`] and the rows are [`SaleListRow`]s because both of
/// them are sums over a SET of documents: a panel that refused to render, or that
/// published a total with one document quietly missing from it, would misstate
/// what the shop is owed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DebtSummary {
    pub total: SetMoney,
    pub count: usize,
    pub oldest: Vec<SaleListRow>,
}

// ---------------------------------------------------------------------------
// M3 purchases: suppliers + product/supplier cost satellite (Slice E).
// Decimal-as-TEXT like finance/inventory. The price alert is derived from
// previous vs current, never stored; `products.cost_price` stays as the
// fallback for products without satellite rows.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Supplier {
    pub id: i64,
    pub name: String,
    pub phone: Option<String>,
    pub notes: Option<String>,
    pub is_active: bool,
    /// Default credit term in days; NULL means no default term.
    pub due_days: Option<i64>,
    /// Audit actor (M5 Phase B, slice S12): who created the supplier and who
    /// last edited it (an edit or the activate/deactivate toggle). The
    /// interface resolves it to a display name; it never shows the id.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// The supplier catalogue under one key, for a client resolving a document's
/// paying party.
///
/// Named because the key is a published contract, and an untyped `json!` lets it
/// change without so much as a type error — which is the failure a type is
/// supposed to prevent.
#[derive(Serialize)]
pub struct SuppliersResponse {
    pub suppliers: Vec<Supplier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductSupplierCost {
    pub id: i64,
    pub product_id: i64,
    pub supplier_id: i64,
    /// Decimal >= 0, stored as TEXT.
    pub current_cost: Decimal,
    pub current_cost_date: NaiveDate,
    /// Decimal >= 0 or NULL when there is no older recorded price.
    pub previous_cost: Option<Decimal>,
    pub previous_cost_date: Option<NaiveDate>,
    pub is_preferred: bool,
    /// The supplier's own code for this product, stored as TEXT.
    pub supplier_sku: Option<String>,
    /// Audit actor (M5 Phase B, slice S12): the acting user of the request
    /// that recorded the cost — a confirm flow stamps the confirming
    /// request's actor, never a fresh one (AC18) — and, when the row was
    /// later shifted, refreshed or preferred, that later request's actor.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

impl ProductSupplierCost {
    /// Derived alert: compare the recorded previous price against the current one.
    pub fn price_alert(&self) -> PriceAlert {
        PriceAlert::compare(self.previous_cost, self.current_cost)
    }
}

/// The cost satellite, filtered by product or by supplier, under one key.
///
/// These are the rows a purchase line's `unit_cost` is resolved from, so the key
/// is an input contract for money: a client that reads `costs` and finds nothing
/// writes a purchase with a cost this application cannot justify. Naming the key
/// is what makes that rename fail the build.
#[derive(Serialize)]
pub struct SupplierCostsResponse {
    pub costs: Vec<ProductSupplierCost>,
}

/// Derived price movement between the previous and current satellite costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum PriceAlert {
    Raised,
    Lowered,
    Unchanged,
}

impl PriceAlert {
    /// `None` previous means no movement to compare yet => Unchanged.
    pub fn compare(previous: Option<Decimal>, current: Decimal) -> Self {
        match previous {
            Some(p) if current > p => Self::Raised,
            Some(p) if current < p => Self::Lowered,
            _ => Self::Unchanged,
        }
    }
}

impl std::fmt::Display for PriceAlert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Raised => write!(f, "Raised"),
            Self::Lowered => write!(f, "Lowered"),
            Self::Unchanged => write!(f, "Unchanged"),
        }
    }
}

/// Service-level input for supplier creation.
#[derive(Debug, Clone, Deserialize)]
pub struct NewSupplier {
    pub name: String,
    pub phone: Option<String>,
    pub notes: Option<String>,
    pub due_days: Option<i64>,
}

/// Service-level patch for supplier edits. `Option<Option<T>>` distinguishes
/// "leave unchanged" (`None`) from "clear" (`Some(None)`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateSupplier {
    pub name: Option<String>,
    pub phone: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub due_days: Option<Option<i64>>,
}

// ---------------------------------------------------------------------------
// M3 purchases domain (mirror orchestrator of M2 sales). Decimal-as-TEXT like
// finance/inventory. The purchase Draft is the pedido: it touches no stock, no
// finance and no satellite cost until confirmed.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum PurchaseStatus {
    Draft,
    Confirmed,
    Cancelled,
}

impl std::fmt::Display for PurchaseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "Draft"),
            Self::Confirmed => write!(f, "Confirmed"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

impl std::str::FromStr for PurchaseStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "confirmed" => Ok(Self::Confirmed),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid purchase status: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Purchase {
    pub id: i64,
    /// `YYYY-PURCH-NNNNNN`, NULL only while Draft, immutable once assigned.
    pub purchase_number: Option<String>,
    pub supplier_id: i64,
    pub status: PurchaseStatus,
    pub payment_type: PaymentType,
    pub purchase_date: NaiveDate,
    /// Required when `payment_type` is Credit, NULL for Cash.
    pub due_date: Option<NaiveDate>,
    pub supplier_invoice_no: Option<String>,
    pub notes: String,
    pub cancel_reason: Option<String>,
    /// Audit actor (M5 Phase B, slice S12): who created the purchase and who
    /// last edited it (a header edit, a line change, the confirm or the
    /// cancel). A line adds no columns of its own: it inherits the purchase's
    /// actor.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub confirmed_at: Option<chrono::NaiveDateTime>,
    pub cancelled_at: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseLine {
    pub id: i64,
    pub purchase_id: i64,
    pub product_id: i64,
    /// Decimal qty > 0, stored as TEXT.
    pub qty: Decimal,
    /// Decimal unit_cost >= 0, frozen at confirm, stored as TEXT.
    pub unit_cost: Decimal,
    /// Decimal tax total stored as TEXT; the purchase-line mirror of
    /// `SaleLine::tax_total`, with the same derived-total rule.
    pub tax_total: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

impl PurchaseLine {
    pub fn subtotal(&self) -> Decimal {
        self.qty * self.unit_cost
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchasePayment {
    pub id: i64,
    pub purchase_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    /// Decimal amount > 0, stored as TEXT.
    pub amount: Decimal,
    pub date: NaiveDate,
    /// Finance transaction this payment created (NULL for historical rows).
    pub transaction_id: Option<i64>,
    /// Refund transaction created when the purchase was cancelled, if any.
    pub refund_transaction_id: Option<i64>,
    /// Audit actor (M5 Phase B, slice S12): the acting user of the request
    /// that recorded the payment — for a cash confirm, the confirming
    /// request's actor; for a supplier payment, the payment request's actor —
    /// never a fresh one (AC18). `updated_by` is the user who linked a refund
    /// to it, when the purchase was cancelled.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// The payments one supplier handover produced, under one key.
///
/// A supplier payment has no grouping document (only customers have receipts),
/// so the response IS the list: one payment per covered purchase, oldest debt
/// first. A client reconciling the handover reads all of it, which is why the key
/// is a named contract rather than something a literal could rename unnoticed.
#[derive(Serialize)]
pub struct SupplierPaymentsResponse {
    pub payments: Vec<PurchasePayment>,
}

/// Service-level input for purchase creation (Draft).
#[derive(Debug, Clone)]
pub struct NewPurchase {
    pub supplier_id: i64,
    pub payment_type: PaymentType,
    pub purchase_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub supplier_invoice_no: Option<String>,
    pub notes: Option<String>,
}

/// Service-level patch for Draft header edits.
#[derive(Debug, Clone, Default)]
pub struct UpdatePurchaseDraft {
    pub supplier_id: Option<i64>,
    pub payment_type: Option<PaymentType>,
    pub purchase_date: Option<NaiveDate>,
    pub due_date: Option<Option<NaiveDate>>,
    pub supplier_invoice_no: Option<Option<String>>,
    pub notes: Option<String>,
}

/// Aggregated purchase view with derived totals (never stored as truth).
///
/// The tax mirror of [`SaleDetail`]: `net_subtotal` and `tax_total` are the two
/// parts of the tax-inclusive `total`, and `paid`/`due` and the payment ceilings
/// are measured against that `total`.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseDetail {
    pub purchase: Purchase,
    pub lines: Vec<PurchaseLine>,
    pub payments: Vec<PurchasePayment>,
    /// `sum(line.subtotal())` — the money before tax.
    pub net_subtotal: Decimal,
    /// `sum(line.tax_total)` — the money the lines' frozen snapshots charge.
    pub tax_total: Decimal,
    /// The tax-inclusive total: the sum of each line's pinned line total.
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

impl PurchaseDetail {
    pub fn payment_status_for(total: Decimal, paid: Decimal) -> PaymentStatus {
        SaleDetail::payment_status_for(total, paid)
    }
}

/// The purchase list the JSON API returns: a detail per document, under one key.
///
/// The tax mirror of the sales list, and for the same two reasons: [`PurchaseDetail`]
/// is the machine contract whose totals are plain decimals, and the key is a
/// published name that a `json!` literal would let a rename ship behind a green
/// build.
#[derive(Serialize)]
pub struct PurchasesResponse {
    pub purchases: Vec<PurchaseDetail>,
}

/// Derived, never stored: one purchase line's cost against the product's
/// stored cost. `Some` only when the line's cost is HIGHER and the stored cost
/// is a real one, which is the state where the product is behind what the
/// supplier is charging. A decrease is deliberately not flagged here: the
/// product drawer's permanent badge covers any disagreement, while this is
/// about a cost that rose.
#[derive(Debug, Clone, Serialize)]
pub struct StaleLineCostView {
    pub line_cost: Decimal,
    pub stored_cost: Decimal,
}

impl StaleLineCostView {
    /// Display form of the line's cost, mirroring `StaleCostView`'s display
    /// methods: a raw `Decimal` renders with its stored scale (e.g. `12.000`
    /// next to `10.00`), and `money_display` is this project's single money
    /// formatting, so both numbers of the gap render consistently.
    pub fn line_cost_display(&self) -> String {
        crate::models::money_display(self.line_cost)
    }

    /// Display form of the stored cost, same rule as above.
    pub fn stored_cost_display(&self) -> String {
        crate::models::money_display(self.stored_cost)
    }
}

/// One purchase line resolved for `/purchases/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseLineView {
    pub id: i64,
    pub product_name: String,
    pub product_sku: String,
    /// The line's product id: the only key a caller needs to resolve the
    /// product's CURRENT state (active flag, stock settings) beyond the names
    /// resolved here.
    pub product_id: i64,
    pub qty: Decimal,
    pub unit_cost: Decimal,
    /// The NET subtotal: `qty * unit_cost`, before any tax.
    pub subtotal: Decimal,
    /// The tax this line froze when it was written. Zero for a product with no
    /// linked tax.
    pub tax_total: Decimal,
    /// The tax-inclusive line total: `round(subtotal + tax_total)`. The
    /// document total is the sum of these, so the page reconciles.
    pub total: Decimal,
    /// The frozen breakdown, empty for a product with no linked tax.
    pub taxes: Vec<LineTaxView>,
    /// The same predicate `confirm` and `cancel` use to decide whether a line
    /// moves stock (`product.kind == Product && product.track_stock`), filled
    /// from the very product read that resolves the name — so any preview
    /// built from this view cannot drift from what those flows will do.
    pub tracks_stock: bool,
    /// Derived, never stored: `Some` only when this line's cost is strictly
    /// higher than the product's stored cost and that stored cost is a real
    /// one (non-zero). Built by `PurchasesService::record_from_detail` in
    /// Rust, because Askama cannot compare decimals or build `Some(...)` in an
    /// expression.
    pub stale_cost: Option<StaleLineCostView>,
}

/// One purchase payment resolved for `/purchases/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct PurchasePaymentView {
    pub id: i64,
    pub account_name: String,
    pub method_name: String,
    pub amount: Decimal,
    pub date: NaiveDate,
}

/// The purchase record page payload: the stored document plus every child with
/// its internal keys replaced by display names (`products.cost_price` stays the
/// fallback the service already applies for an empty line cost). Totals stay
/// derived.
///
/// `money` and `total_refusal` are one fact in two halves, exactly as on
/// [`SaleRecord`] and for the same reason: the purchase record page is the
/// operator's way into a document whose lines cannot be added up, so it renders
/// the document and states the refusal instead of answering an error.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseRecord {
    pub purchase: Purchase,
    /// Purchases store only the supplier id; the name is resolved for display.
    pub supplier_name: String,
    pub lines: Vec<PurchaseLineView>,
    pub payments: Vec<PurchasePaymentView>,
    /// The document's money, or `None` when its lines cannot be added up.
    pub money: Option<RecordMoney>,
    /// The rule that refused [`Self::money`], when it is `None`.
    pub total_refusal: Option<PriceRefusal>,
    /// Sum of `qty` over the lines whose `tracks_stock` predicate holds — the
    /// units `confirm`/`cancel` will actually move (receiving-desk T2). Built
    /// in Rust from the same per-line flags the record already computes, so
    /// any effects preview rendered from it cannot drift from those flows.
    /// The units the stock flows will move, as a SET SUM over the document's
    /// stock-tracking lines — so it is a [`SetMoney`], not a bare `Decimal`, and
    /// for the same reason the money figures are.
    ///
    /// A QUANTITY is bounded by its own argument, and the argument is the one
    /// this whole change exists on: a per-line bound never covers a per-document
    /// fold. `qty` is operator-typed, nothing above it is bounded but
    /// representability, and a line's AMOUNT says nothing about it — `4e28` units
    /// at a unit cost of `0` is a line of amount `0` and a document total of `0`,
    /// which every money bound in this codebase carries without complaint. The
    /// unit count is the one figure on this record that can still leave the range,
    /// so it refuses the same way, through the same rule, and the page says so
    /// instead of printing a number no operator can act on.
    pub tracked_units: SetMoney,
}

/// Format `YYYY-PURCH-NNNNNN` with zero-padded 6-digit sequence.
pub fn format_purchase_number(year: i32, seq: i64) -> String {
    format!("{year}-PURCH-{seq:06}")
}

/// Server-side filter for the purchases list (redesign-interface N5). The same
/// shape as `SaleListFilter`; `supplier` is the typed party name, resolved by the
/// service against the suppliers table (normalized) into `supplier_ids`, and the
/// repository narrows the document query by those ids. `number` matches partially.
#[derive(Debug, Clone, Default)]
pub struct PurchaseListFilter {
    pub status: Option<PurchaseStatus>,
    /// The typed party name, resolved by the service into `supplier_ids`.
    pub supplier: Option<String>,
    /// Matching supplier ids, set by the service; `Some(empty)` matches nothing.
    pub supplier_ids: Option<Vec<i64>>,
    pub number: Option<String>,
    /// Inclusive lower bound on `purchase_date`.
    pub from: Option<NaiveDate>,
    /// Inclusive upper bound on `purchase_date`.
    pub to: Option<NaiveDate>,
}

// ---------------------------------------------------------------------------
// Documents index (cross-department read layer, slice 1 of 2): the shared types
// the sales and purchases repositories already project and the receipts/stock
// families compose later. One row per stored document; `DocumentKind` is the
// feed's vocabulary (declaration order is the last tiebreak, so it is stable)
// and `DocumentGroup` is the filter form's coarser one.
// ---------------------------------------------------------------------------

/// One family of documents the cross-department index reads: one table per
/// variant. Declaration order is the feed's last tiebreak, so it is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DocumentKind {
    Sale,
    SalePayment,
    Purchase,
    PurchasePayment,
    StockMovement,
    Receipt,
}

impl DocumentKind {
    /// Every family, in declaration order.
    pub const ALL: &'static [DocumentKind] = &[
        Self::Sale,
        Self::SalePayment,
        Self::Purchase,
        Self::PurchasePayment,
        Self::StockMovement,
        Self::Receipt,
    ];

    /// The stable token the filter form and the URL use: "sale",
    /// "sale_payment", "purchase", "purchase_payment", "stock_movement",
    /// "receipt".
    pub fn token(&self) -> &'static str {
        match self {
            Self::Sale => "sale",
            Self::SalePayment => "sale_payment",
            Self::Purchase => "purchase",
            Self::PurchasePayment => "purchase_payment",
            Self::StockMovement => "stock_movement",
            Self::Receipt => "receipt",
        }
    }

    /// Parse a `token()`; `None` for an unknown or empty token. The drawer
    /// route's `{kind}` path segment is the consumer: the URL segment resolves
    /// back to exactly one family or the request is a 404.
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.token() == token)
    }

    /// The catalog code that READS this family — the single mapping the page
    /// (`permitted_kinds`), the drawer route's per-family narrowing and the
    /// kernel agreement test must share, so a family can never open under a
    /// code the owning tier did not choose.
    pub fn read_code(&self) -> &'static str {
        match self {
            Self::Sale | Self::SalePayment => "sales.read",
            Self::Purchase | Self::PurchasePayment => "purchases.read",
            Self::StockMovement => "inventory.read",
            Self::Receipt => "customers.read",
        }
    }

    /// The Spanish label a row and a filter option show: "Venta",
    /// "Pago de venta", "Compra", "Pago de compra", "Movimiento de stock",
    /// "Recibo de cliente".
    pub fn label(&self) -> &'static str {
        match self {
            Self::Sale => "Venta",
            Self::SalePayment => "Pago de venta",
            Self::Purchase => "Compra",
            Self::PurchasePayment => "Pago de compra",
            Self::StockMovement => "Movimiento de stock",
            Self::Receipt => "Recibo de cliente",
        }
    }
}

/// The four groups the operator filters by — the vocabulary of the request
/// ("ventas, compras, movimientos de stock, pagos") over six families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DocumentGroup {
    Sales,
    Purchases,
    Stock,
    Payments,
}

impl DocumentGroup {
    pub const ALL: &'static [DocumentGroup] =
        &[Self::Sales, Self::Purchases, Self::Stock, Self::Payments];

    /// Tokens: "sales", "purchases", "stock", "payments".
    pub fn token(&self) -> &'static str {
        match self {
            Self::Sales => "sales",
            Self::Purchases => "purchases",
            Self::Stock => "stock",
            Self::Payments => "payments",
        }
    }

    /// Labels: "Ventas", "Compras", "Movimientos de stock", "Pagos".
    pub fn label(&self) -> &'static str {
        match self {
            Self::Sales => "Ventas",
            Self::Purchases => "Compras",
            Self::Stock => "Movimientos de stock",
            Self::Payments => "Pagos",
        }
    }

    /// The families the group covers. The four groups PARTITION the six
    /// families: every family is listed under exactly one option, so the
    /// partition is the single classification and expanding two
    /// selected options can never list the same row twice. Sales -> [Sale];
    /// Purchases -> [Purchase]; Stock -> [StockMovement]; Payments ->
    /// [SalePayment, PurchasePayment, Receipt] — the three payment families
    /// are one option because that is the vocabulary the operator filters
    /// by, and choosing "Ventas" means the sale documents, not their money.
    pub fn kinds(&self) -> &'static [DocumentKind] {
        match self {
            Self::Sales => &[DocumentKind::Sale],
            Self::Purchases => &[DocumentKind::Purchase],
            Self::Stock => &[DocumentKind::StockMovement],
            Self::Payments => &[
                DocumentKind::SalePayment,
                DocumentKind::PurchasePayment,
                DocumentKind::Receipt,
            ],
        }
    }

    /// Parse a `token()`; `None` for an unknown or empty token.
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|group| group.token() == token)
    }
}

/// What the page asks the index for. `kinds` is the page's decision, already
/// narrowed by the principal's permissions in the route.
#[derive(Debug, Clone, Default)]
pub struct DocumentFilter {
    /// The families to read. Empty reads nothing.
    pub kinds: Vec<DocumentKind>,
    /// Audit-actor ids the filter accepts; `Some(empty)` matches nothing.
    pub actor_ids: Option<Vec<i64>>,
    /// Inclusive lower bound on the family's date column.
    pub from: Option<NaiveDate>,
    /// Inclusive upper bound on the family's date column.
    pub to: Option<NaiveDate>,
    /// Free text: the document's identifier/reference and its counterpart.
    pub search: Option<String>,
}

impl DocumentFilter {
    /// The per-family bounds for a read capped at `limit` rows. The method
    /// called on the repository decides the family, so `kinds` plays no part
    /// in the per-family query.
    pub fn query(&self, limit: usize) -> DocumentQuery {
        DocumentQuery {
            actor_ids: self.actor_ids.clone(),
            from: self.from,
            to: self.to,
            search: self.search.clone(),
            limit,
        }
    }
}

/// What ONE family read accepts: the filter's bounds plus the row cap (the
/// method called decides the family, so the family list plays no part).
#[derive(Debug, Clone)]
pub struct DocumentQuery {
    pub actor_ids: Option<Vec<i64>>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub search: Option<String>,
    /// Maximum rows THIS family read returns.
    pub limit: usize,
}

/// One row of the index: one stored document projected to the facts the feed
/// shows. `amount` is the derived money (summed in Rust, never with SQL) and
/// `quantity` is the stock magnitude — the only family with no money.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentRow {
    pub kind: DocumentKind,
    /// The row's own id.
    pub id: i64,
    /// The owning document's id: the drill-down target.
    pub owner_id: i64,
    /// The identifier the operator reads.
    pub reference: String,
    /// The counterpart: customer, supplier or product.
    pub party: String,
    pub date: NaiveDate,
    /// The status/detail pill.
    pub detail: String,
    /// `None` for a family with no money (a stock movement), and `None` with a
    /// `total_refusal` for a document whose lines cannot be added up. The
    /// second case is NOT the first: the row keeps its place in the index and
    /// states the rule, because one document that cannot be totaled must not cost
    /// the operator the rest of the page.
    pub amount: Option<Decimal>,
    /// The rule that refused `amount` on a family that HAS money. Never set
    /// without `amount` being `None`, and never set on a family with no money.
    pub total_refusal: Option<PriceRefusal>,
    pub quantity: Option<Decimal>,
    /// The audit actor the row records.
    pub created_by: i64,
}

/// The index's row cap: the newest N documents the feed shows. A page that
/// hits it says so instead of pretending the history ended.
pub const DOCUMENTS_PAGE_LIMIT: usize = 200;

/// One low-stock product with a chosen supplier from the cost satellite.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseSuggestion {
    pub product: Product,
    /// The product's level, or the rule that stopped the sum. The reorder panel
    /// is a list of set sums like every other list in this change, so the product
    /// whose movements cannot be added up keeps its row and states the rule.
    pub stock: SetMoney,
    /// The suggested reorder quantity, and `None` for a refused level.
    ///
    /// `None`, not `0`: a suggestion needs the level it comes from, and a `0`
    /// beside a refusal would read as "reorder nothing" — a claim derived from a
    /// figure nobody can state. `None` is the honest "no suggestion exists", and
    /// the row states the rule beside it.
    pub suggested_qty: Option<Decimal>,
    pub supplier_id: i64,
    pub supplier_name: String,
    pub unit_cost: Decimal,
    /// `suggested_qty * unit_cost`, and `None` with it — the subtotal is a
    /// multiplication of the suggestion, so it is absent exactly when the
    /// suggestion is.
    pub subtotal: Option<Decimal>,
}

/// Low-stock product with no satellite row: never silently dropped, returned
/// in the `without_supplier` list so the user can pick a one-off supplier.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseSuggestionWithoutSupplier {
    pub product: Product,
    /// The level, or the rule — the same shape and the same reason as
    /// [`PurchaseSuggestion::stock`].
    pub stock: SetMoney,
    /// `None` for a refused level, for the same reason as on the costed row.
    pub suggested_qty: Option<Decimal>,
}

/// The pedido suggestion: costed low-stock lines plus the unsourced ones.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PurchaseSuggestions {
    pub suggestions: Vec<PurchaseSuggestion>,
    pub without_supplier: Vec<PurchaseSuggestionWithoutSupplier>,
}

// ---------------------------------------------------------------------------
// M4 customers (Slice K1). Customer CRUD only: the sales link and the derived
// balance/ageing arrive in a later slice. Decimal-as-TEXT like the rest of the
// project. A name is not unique on purpose; duplicates are reported as a
// warning instead of blocking. is_walkin marks the single seeded cash default
// ("Consumidor final"), which can never be deleted or deactivated.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Customer {
    pub id: i64,
    pub name: String,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub tax_id: Option<String>,
    pub notes: Option<String>,
    /// The seeded cash default. Exactly one row has this set.
    pub is_walkin: bool,
    pub is_active: bool,
    /// Decimal >= 0 stored as TEXT; NULL means no limit.
    pub credit_limit: Option<Decimal>,
    /// Default credit term in days; NULL means no default term.
    pub due_days: Option<i64>,
    /// Audit actor (M5 Phase B, slice S11): who created the customer and who
    /// last edited it. The seeded walk-in predates the audit, so its actor is
    /// the migration's sentinel.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// Service-level input for customer creation. `is_walkin` is accepted only when
/// no walk-in exists yet, which after the seed means never.
#[derive(Debug, Clone, Deserialize)]
pub struct NewCustomer {
    pub name: String,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub tax_id: Option<String>,
    pub notes: Option<String>,
    #[serde(default)]
    pub is_walkin: bool,
    /// None means no limit.
    pub credit_limit: Option<Decimal>,
    /// None means no default term.
    pub due_days: Option<i64>,
}

/// Service-level patch for customer edits. `Option<Option<T>>` distinguishes
/// "leave unchanged" (`None`) from "clear" (`Some(None)`). `is_walkin` is not
/// editable.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateCustomer {
    pub name: Option<String>,
    pub phone: Option<Option<String>>,
    pub address: Option<Option<String>>,
    pub tax_id: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub credit_limit: Option<Option<Decimal>>,
    pub due_days: Option<Option<i64>>,
}

/// Outcome of creating a customer: the new row plus any customers that already
/// had that exact name, so the interface can warn without blocking (AC15).
#[derive(Debug, Clone, Serialize)]
pub struct CustomerCreateResult {
    pub customer: Customer,
    pub name_matches: Vec<Customer>,
}

// ---------------------------------------------------------------------------
// M4 customers (Slice K3). The receivable is derived from sales and payments,
// so these reads live in `SalesService`: customers sits above sales, and the
// reverse would be circular. Decimal-as-TEXT like the rest of the project, so
// the buckets and the running balance are summed in Rust, never with SQL SUM.
// ---------------------------------------------------------------------------

/// Ageing of a derived receivable against an explicit `as_of` date. Each sale
/// with `due > 0` falls in exactly one bucket by how many days late it is.
///
/// Every bucket is a [`SetMoney`] and not a bare `Decimal`, because a bucket is a
/// SET SUM like any other: it is a sum over the documents that fall in it, so it
/// is bounded by a checked accumulation and refuses rather than publishing a
/// figure. The alternative — a `Decimal` plus a `#[serde(skip)]` flag — put four
/// genuine-looking zeros on the wire with no reason attached, and a client read
/// that as "owes nothing". There is now nothing on this type that can be zero
/// BECAUSE a sum was refused: a refused bucket has no amount at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Ageing {
    /// Not yet due, due today, or no due date at all.
    pub current: SetMoney,
    /// 1 to 30 days past the due date.
    pub overdue_1_30: SetMoney,
    /// 31 to 60 days past the due date.
    pub overdue_31_60: SetMoney,
    /// More than 60 days past the due date.
    pub overdue_61_plus: SetMoney,
}

impl Ageing {
    /// The four buckets as ONE figure — or the rule that stopped the
    /// accumulation.
    ///
    /// Two bounds live here and both are needed, because the buckets are a
    /// PARTITION of one receivable and a bound that holds inside a bucket says
    /// nothing about the sum ACROSS the partition: two documents of `4e28`, one
    /// not yet due and one ten days late, each fit their own bucket and together
    /// are `8e28`, which `Decimal` cannot carry. A refused bucket refuses the
    /// total (a total over a refused part is a refusal), and the cross-bucket sum
    /// is `checked_add` for the same reason the per-bucket sums are: the raw `+`
    /// here is a panic, and the operator's page is where it would land.
    pub fn total(&self) -> SetMoney {
        if let Some(refusal) = self.refusal() {
            return SetMoney::refused(refusal);
        }
        let mut sum = Decimal::ZERO;
        for bucket in self.buckets() {
            // Every bucket carries an amount here: `refusal()` found none.
            let amount = bucket.amount.unwrap_or(Decimal::ZERO);
            match sum.checked_add(amount) {
                Some(next) => sum = next,
                None => return SetMoney::refused(PriceRefusal::DocumentTotalTooLarge),
            }
        }
        SetMoney::amount(sum)
    }

    /// The rule that stopped this ageing, if any of the buckets carries one.
    /// Derived, never stored: a flag beside the figures is one more thing that
    /// can disagree with them.
    pub fn refusal(&self) -> Option<PriceRefusal> {
        self.buckets().iter().find_map(|bucket| bucket.refusal)
    }

    /// The four buckets in grid order.
    pub fn buckets(&self) -> [SetMoney; 4] {
        [
            self.current,
            self.overdue_1_30,
            self.overdue_31_60,
            self.overdue_61_plus,
        ]
    }
}

/// One row of the receivables view: a customer with a non-zero derived balance
/// and the ageing of that balance as of the requested date. Names stay with
/// `CustomerService`; routes compose the two reads.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CustomerAgeing {
    pub customer_id: i64,
    pub balance: SetMoney,
    pub ageing: Ageing,
}

/// What produced a statement entry: a confirmed credit sale (a debit) or a
/// payment received on one of those sales (a credit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum StatementEntryKind {
    Sale,
    Payment,
}

/// One line of a customer statement. `balance` is the running balance after
/// applying this entry, so the last entry always lands on the statement total.
///
/// `debit` and `balance` are [`SetMoney`]s because both are derived from document
/// totals: a document whose lines cannot be added up has no debit, and from that
/// entry on the running balance cannot be stated either. `credit` is not — a
/// payment row carries its own stored amount, which is always representable.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatementEntry {
    pub date: NaiveDate,
    pub kind: StatementEntryKind,
    /// The document the entry belongs to (`YYYY-SALE-NNNNNN`). A payment carries
    /// the sale it was applied to, which keeps tied dates orderable.
    pub document_number: Option<String>,
    pub description: String,
    pub debit: SetMoney,
    pub credit: Decimal,
    pub balance: SetMoney,
}

/// Derived account statement of one customer: the full confirmed-credit ledger
/// with its running balance, plus the ageing of the same receivable as of
/// `as_of`. Cancelled sales contribute nothing to either side.
///
/// Every figure here is a sum over a SET of documents, so each one is a
/// [`SetMoney`]: a statement that dropped the document it could not total would
/// state a balance the operator cannot audit, and one that refused to render
/// would hide every OTHER document on the page behind it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CustomerStatement {
    pub customer_id: i64,
    pub balance: SetMoney,
    pub as_of: NaiveDate,
    pub ageing: Ageing,
    pub entries: Vec<StatementEntry>,
}

/// The statement endpoint's answer: the customer the module owns beside the
/// ledger derived for them.
///
/// Two keys and not one, because the statement does not carry the customer's own
/// fields — the party name the operator reads comes from the entity, and a
/// client that only had the ledger would have to make a second request to label
/// what it is looking at. The statement also states its own `customer_id`, and
/// keeping the two here is what lets a client tell whose statement it received
/// without trusting the URL it asked for.
#[derive(Serialize)]
pub struct CustomerStatementResponse {
    pub customer: Customer,
    pub statement: CustomerStatement,
}

// ---------------------------------------------------------------------------
// M4 customers (Slice L). A customer receipt is the document a single handover
// of money produces: it groups one `sale_payments` row per credit sale the
// amount covered, applied oldest debt first. Each grouped payment still belongs
// to its sale and keeps its own finance link, so traceability is untouched; the
// receipt posts no movement of its own. There is NO stored total: the amount
// handed over is derived as SUM(allocations), so an interrupted collection can
// leave fewer payments but never a receipt claiming more than it applied.
// Decimal-as-TEXT like the rest of the project.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerReceipt {
    pub id: i64,
    pub customer_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    pub date: NaiveDate,
    /// Optional free text (trimmed, <= 256 chars), NULL when empty.
    pub notes: Option<String>,
    /// Audit actor (M5 Phase B, slice S11): the acting user of the collection
    /// request that produced the receipt. The receipt has no edit path, so
    /// `updated_by` stays NULL.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
}

/// Service-level input for creating a receipt. There is no total field: the
/// collected amount is a plan input, not a stored claim; what the document
/// applied is derived from its payments. There is no account field either: the
/// account is derived from the method, which belongs to exactly one account.
#[derive(Debug, Clone)]
pub struct NewReceipt {
    pub customer_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    pub date: NaiveDate,
    pub notes: Option<String>,
}

/// One receipt with the payments it groups. `allocations` are the
/// `sale_payments` rows carrying the receipt id, one per covered sale and each
/// with its own `transaction_id`.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptDetail {
    pub receipt: CustomerReceipt,
    pub allocations: Vec<SalePayment>,
    /// Derived, never stored: `SUM(allocations.amount)`, i.e. exactly what was
    /// handed over and applied. A stored copy could disagree with the payments;
    /// this one is computed from them.
    pub total: Decimal,
    /// Account name resolved for display through the account read path.
    pub account_name: String,
    /// Payment-method name resolved for display through the finance read path.
    pub method_name: String,
}

/// The receipts of ONE customer, each with the payments it groups, under one key.
///
/// There is no unbounded receipt dump anywhere in this module — the customer is
/// mandatory — so the key is also the scope of the answer, and a client reading
/// it is reading exactly one customer's collections. That is why it is named:
/// nothing in an untyped literal would notice the scope widening.
#[derive(Serialize)]
pub struct ReceiptsResponse {
    pub receipts: Vec<ReceiptDetail>,
}

// ---------------------------------------------------------------------------
// M5 identity kernel (Slice S1a). Users and sessions only: RBAC roles,
// permissions and the audit columns arrive in later slices. The ordinary
// `User` read never carries `password_hash`; `UserWithHash` exists only for
// the credential-verification path and is deliberately not `Serialize`, so
// the PHC string can never ride a response. Sessions store the sha256 digest
// of the cookie token (`token_hash`), never the token itself.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub is_active: bool,
    pub must_change_password: bool,
    pub last_login_at: Option<chrono::NaiveDateTime>,
    /// Audit actor (M5 Phase B, slice S13): who created the user and who last
    /// edited it (an activation toggle, the administrator reset or the
    /// user's own password change). NULL means the system created the row —
    /// the migration's sentinel, the bootstrap administrator — and the
    /// interface renders that honestly instead of inventing a name.
    pub created_by: Option<i64>,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// A user together with its argon2id PHC string. Constructed only by the
/// authentication read paths (`find_with_hash_by_*`) and consumed only by
/// `IdentityService` when verifying a credential; it must never be serialized.
/// `Debug` is hand-written so a log line can never print the stored verifier.
#[derive(Clone)]
pub struct UserWithHash {
    pub user: User,
    pub password_hash: String,
}

impl std::fmt::Debug for UserWithHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserWithHash")
            .field("user", &self.user)
            .field("password_hash", &"<redacted>")
            .finish()
    }
}

/// Service-level input for user creation. `password_hash` arrives already
/// hashed; plaintext passwords never enter the repository layer. `Debug` is
/// hand-written so logs and test output cannot print the hash material.
#[derive(Clone)]
pub struct NewUser {
    pub username: String,
    pub display_name: String,
    pub password_hash: String,
    pub must_change_password: bool,
}

impl std::fmt::Debug for NewUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewUser")
            .field("username", &self.username)
            .field("display_name", &self.display_name)
            .field("password_hash", &"<redacted>")
            .field("must_change_password", &self.must_change_password)
            .finish()
    }
}

// No `Serialize`: the session row carries `token_hash`, and nothing in this
// slice serializes it — any future response view must be a dedicated DTO.
#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub id: i64,
    /// sha256 (base64url) of the cookie token; the raw token is never stored.
    pub token_hash: String,
    pub user_id: i64,
    pub created_at: chrono::NaiveDateTime,
    pub expires_at: chrono::NaiveDateTime,
    pub last_seen_at: chrono::NaiveDateTime,
    pub revoked_at: Option<chrono::NaiveDateTime>,
    pub user_agent: Option<String>,
}

/// Service-level input for session creation. `expires_at` and the matching
/// `last_seen_at` come from the injected clock.
#[derive(Debug, Clone)]
pub struct NewSession {
    pub token_hash: String,
    pub user_id: i64,
    pub expires_at: chrono::NaiveDateTime,
    pub last_seen_at: chrono::NaiveDateTime,
    pub user_agent: Option<String>,
}

/// What request authentication resolves: the acting user plus the live
/// session row. No secrets: the token itself never survives login.
#[derive(Debug, Clone)]
pub struct ResolvedSession {
    pub user: User,
    pub session: Session,
}

// ---------------------------------------------------------------------------
// RBAC (M5 identity kernel, slice S2)
// ---------------------------------------------------------------------------

/// A role row. `code` is the machine name (unique, `^[a-z][a-z0-9_]*$`),
/// `name` is the Spanish label the interface shows. `is_system` marks the
/// protected role: the database triggers refuse to delete it, rename it or
/// remove its permission rows.
#[derive(Debug, Clone)]
pub struct Role {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    /// Audit actor (M5 Phase B, slice S13): the seeded roles are attributed to
    /// the migration's sentinel; a role created through the screen names its
    /// author, and an edit of its details or matrix names the editor.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// A permission-catalog row: `<module>.<action>` (or `<module>.<resource>
/// .<action>`) with the human-readable pieces the S4 permission matrix shows.
/// The catalog is data seeded by migration; the drift test in
/// `security/authz.rs` keeps it byte-identical to the compiled catalog.
#[derive(Debug, Clone)]
pub struct Permission {
    pub id: i64,
    pub code: String,
    pub module: String,
    pub action: String,
    pub description: String,
    pub created_at: chrono::NaiveDateTime,
}

/// One user together with the roles they hold (S3 users screen): the list
/// read the administration page renders — username, display name, state and
/// the Spanish role labels. Carries no credential material: it wraps the
/// ordinary `User` read.
#[derive(Debug, Clone)]
pub struct UserWithRoles {
    pub user: User,
    pub roles: Vec<Role>,
    /// The grant trail (slice S13): for every role the user holds, who
    /// granted it and when — the `user_roles` columns the RBAC slice has
    /// recorded since S2 and the interface never showed. Same order as
    /// `roles`.
    pub grants: Vec<RoleGrant>,
}

/// One grant of a role to a user: the role, the granter's user id and the
/// instant the grant was recorded. The display name is resolved in the wiring
/// layer, like every other audit display.
#[derive(Debug, Clone)]
pub struct RoleGrant {
    pub role: Role,
    pub granted_by: i64,
    pub granted_at: chrono::NaiveDateTime,
}

/// Service-level input for assigning a role to a user. `granted_by` records
/// who made the privilege change (spec: granting a role is itself one).
#[derive(Debug, Clone)]
pub struct NewUserRole {
    pub user_id: i64,
    pub role_id: i64,
    pub granted_by: i64,
}

/// Service-level input for creating a role (S4 roles screen). `code` is the
/// machine name the schema CHECK validates (`^[a-z][a-z0-9_]*$`, 2-64); the
/// service pre-checks the shape so the operator reads the rule in Spanish
/// before the write is attempted, with the CHECK as the backstop.
#[derive(Debug, Clone)]
pub struct NewRole {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
}

/// One role together with the usernames of the users that hold it (S4 roles
/// list and the AC15 refusal): the count the list shows AND the names a
/// blocked deletion reports. Holders of ANY state count — `user_roles`
/// RESTRICT blocks the deletion for inactive holders too.
#[derive(Debug, Clone)]
pub struct RoleWithHolders {
    pub role: Role,
    pub holders: Vec<String>,
}

/// A role's permission matrix read (S4): the role, the whole seeded catalog
/// (the 23 rows the matrix renders, each with its Spanish description) and
/// the ids of the permissions the role currently holds. The screen groups the
/// catalog by module and ticks the held ids.
#[derive(Debug, Clone)]
pub struct RoleMatrix {
    pub role: Role,
    pub catalog: Vec<Permission>,
    pub held_ids: Vec<i64>,
}

impl ReceiptDetail {
    pub fn new(receipt: CustomerReceipt, allocations: Vec<SalePayment>) -> Self {
        let total = allocations.iter().map(|payment| payment.amount).sum();
        Self {
            receipt,
            allocations,
            total,
            account_name: String::new(),
            method_name: String::new(),
        }
    }

    /// Attach the display names the receipt list shows, so the template never
    /// prints the internal account/method keys.
    pub fn with_names(mut self, account_name: String, method_name: String) -> Self {
        self.account_name = account_name;
        self.method_name = method_name;
        self
    }
}

// ---------------------------------------------------------------------------
// Purchase returns and credit notes (odd/tasks/purchase-returns-and-credit-notes.md)
//
// Two document families that reverse part of a confirmed purchase or sale, which
// is what the annulment path cannot express: cancel is all-or-nothing and it
// discards the document rather than reversing part of it. They are two mirrored
// sets of types rather than one set with a discriminator, because the repository
// already mirrors every domain this way and a unified table would be the first
// polymorphism in the persistence layer.
//
// THIS IS THE MODEL LAYER ONLY. The columns are migration 40 (purchase returns)
// and migration 41 (credit notes); the rules about what a return DOES — the
// refund cap, the stock movement, the freeze of the parent's price — belong to
// the service units that follow, and the money fields below are figures a service
// computes and hands over, never figures a model derives on its own.
//
// `Decimal` here is exactly what `PurchaseLine` and `SaleLine` carry: the column
// is TEXT and the conversion happens once, at the repository boundary, through the
// same `parse_decimal` every other money column already goes through.
// ---------------------------------------------------------------------------

/// The three states a return shares with the documents it reverses. `Display`
/// writes the capitalized words the migration's CHECK accepts, and `FromStr`
/// folds case and takes both spellings of cancelled, because a second dialect
/// for the same three values would mean a row one reader wrote fails the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum PurchaseReturnStatus {
    Draft,
    Confirmed,
    Cancelled,
}

impl std::fmt::Display for PurchaseReturnStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "Draft"),
            Self::Confirmed => write!(f, "Confirmed"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

impl std::str::FromStr for PurchaseReturnStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "confirmed" => Ok(Self::Confirmed),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid purchase return status: {s}")),
        }
    }
}

/// The credit note's own status, a separate type from [`PurchaseReturnStatus`]
/// rather than a shared one: the two families are two tables with two CHECKs, and
/// a shared enum would let a `Draft` sale return be constructed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum CustomerReturnStatus {
    Draft,
    Confirmed,
    Cancelled,
}

impl std::fmt::Display for CustomerReturnStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "Draft"),
            Self::Confirmed => write!(f, "Confirmed"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

impl std::str::FromStr for CustomerReturnStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "confirmed" => Ok(Self::Confirmed),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid customer return status: {s}")),
        }
    }
}

/// A purchase return: the business sends goods BACK to a supplier. Stock goes out
/// and money comes in, which is the only thing that distinguishes it from a
/// purchase — the direction, and the fact that a return is partial.
///
/// **`payment_type` is deliberately absent.** `purchases` carries the flag and the
/// refund cap reads it, but a return's refunds are determined entirely by the
/// PARENT's payment rows — which account each payment came from, how much was
/// collected, when — so a second copy here would be a value that could disagree
/// with the rows it summarizes. The service reads the parent's. There is also no
/// `due_date` and no `supplier_invoice_no`: a return is dated when it is made and
/// is evidenced by the purchase it reverses, so both would be a second place for
/// the same fact to be wrong.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseReturn {
    pub id: i64,
    /// `YYYY-PRET-NNNNNN`, NULL only while Draft, immutable once assigned —
    /// the same contract as `purchase_number`, because it is the same kind of
    /// counter in the same table.
    pub return_number: Option<String>,
    /// Copied from the parent purchase rather than resolved through it, so the
    /// return is self-contained on its own page. History survives: a supplier is
    /// deactivated, not deleted.
    pub supplier_id: i64,
    /// The confirmed purchase this reverses. `RESTRICT` in the database: a return
    /// is evidence ABOUT a purchase, and deleting the evidence because the subject
    /// was deleted is the wrong direction.
    pub purchase_id: i64,
    pub status: PurchaseReturnStatus,
    /// The day the return is MADE, not the parent's purchase date: the goods
    /// leave today and the money arrives today, and dating the document to the
    /// purchase would put stock movements in the past.
    pub return_date: NaiveDate,
    pub notes: String,
    pub cancel_reason: Option<String>,
    /// Audit actor: who created the return and who last edited it. A line adds no
    /// columns of its own — it inherits the return's actor, as a purchase line
    /// inherits the purchase's.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub confirmed_at: Option<chrono::NaiveDateTime>,
    pub cancelled_at: Option<chrono::NaiveDateTime>,
}

/// One line of a purchase return: a quantity OF a parent purchase line, at that
/// line's cost.
///
/// **`product_id` is deliberately absent**, where `PurchaseLine` carries one: the
/// return line names the parent LINE, and the product is one read away through it.
/// A second copy of the product could name a different product than the line it
/// claims to return, and nothing in the schema would notice.
///
/// There is also no `tax_total`, because the column does not exist. The tax
/// snapshot is not carried onto a return, so there is nothing here to separate
/// from the line's money.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseReturnLine {
    pub id: i64,
    pub return_id: i64,
    /// The parent line being returned. `RESTRICT`, and `UNIQUE` per return with
    /// this column: the return's quantity is a quantity of THAT line, and
    /// splitting one parent line into two return lines has no defined answer —
    /// the same argument that makes a purchase refuse a repeated product.
    pub purchase_line_id: i64,
    /// Decimal qty > 0, stored as TEXT.
    pub qty: Decimal,
    /// Decimal unit_cost >= 0, FROZEN from the parent line when this line was
    /// added, for the reason migration 39 froze the tax breakdown: a fact a later
    /// rule could move must not be able to rewrite a document. A return is always
    /// at the purchase price — `product_supplier_costs` holds one price per
    /// (product, supplier) and that price history is the fact being recorded — so
    /// this is a copy of a frozen value rather than a new price, and the form has
    /// no price field at all.
    pub unit_cost: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

impl PurchaseReturnLine {
    /// The line's money: the returned quantity at the frozen cost. Multiplication
    /// only, and no rounding — a stored cost finer than two decimals is a fact
    /// about the purchase, and the document total is a sum of these computed by a
    /// service through a checked fold, never here.
    pub fn subtotal(&self) -> Decimal {
        self.qty * self.unit_cost
    }
}

/// One refund of a purchase return: money coming BACK from the supplier, so it is
/// an Income and the overdraft guard never fires on it.
///
/// **Both link columns are present from the start**, as `purchase_payments` and
/// `sale_payments` were in migration 19, rather than adding `refund_transaction_id`
/// by `ALTER` when the reversal path arrives. `transaction_id` is NULL for a
/// historical row, `refund_transaction_id` is NULL until something is reversed,
/// and a return that was itself reversed has BOTH — a state a single nullable
/// column could not represent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseReturnPayment {
    pub id: i64,
    pub return_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    /// Decimal amount > 0, stored as TEXT. Capped at what the parent purchase has
    /// actually collected, so a return on a confirmed-but-unpaid purchase writes
    /// no payment row at all — the return still exists to return the goods.
    pub amount: Decimal,
    pub date: NaiveDate,
    /// Finance transaction this refund created (NULL for historical rows).
    pub transaction_id: Option<i64>,
    /// Refund transaction created when THIS RETURN was reversed, if it was.
    pub refund_transaction_id: Option<i64>,
    /// Audit actor: the acting user of the request that recorded the refund, and
    /// `updated_by` the user who linked a reversal to it.
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// Aggregated purchase-return view with derived totals (never stored as truth).
///
/// The same shape as [`PurchaseDetail`], with the two differences the migrations
/// force. There is no `tax_total`, and no field standing in for it: a return line
/// freezes no tax, so `net_subtotal` and `total` are the same figure and
/// `net_subtotal` is here so a record page has one name for "the money before
/// tax" that does not have to change if a return ever does start freezing one.
/// `total` is still the figure `paid`/`due` and the refund cap are measured
/// against, as they are on a purchase.
///
/// `paid` is money REFUNDED to the business, which is the mirror of the parent's
/// `paid`; the direction of the money is the only thing that changes. And there
/// is no `payment_type` — see [`PurchaseReturn`] for why the parent's is the only
/// copy.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseReturnDetail {
    pub purchase_return: PurchaseReturn,
    pub lines: Vec<PurchaseReturnLine>,
    pub payments: Vec<PurchaseReturnPayment>,
    /// `sum(line.subtotal())` — the money before tax, which here is all of it.
    pub net_subtotal: Decimal,
    /// The document total: the tax-inclusive figure, and with no frozen tax on a
    /// line the same number as `net_subtotal`.
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

/// A credit note: the customer sends goods BACK to the business. Stock comes in
/// and money goes out, so the refund is an Expense and the overdraft guard DOES
/// fire on it — the one behavioural asymmetry with the purchase return, and the
/// reason the two are separate types rather than one parameterised over a sign.
///
/// **`payment_type` is deliberately absent**, for the same reason as on
/// [`PurchaseReturn`]: the parent sale's payment rows are what a refund is
/// computed from, and a second copy of the flag here could disagree with them.
///
/// The document is called `CustomerReturn` rather than `SaleReturn` on purpose.
/// `SaleReturn` is the stock movement reason, it describes a physical event, and
/// it keeps that name; a document and a movement must not share a word in code
/// either, since the ambiguity is exactly what the naming decision removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerReturn {
    pub id: i64,
    /// `YYYY-SRET-NNNNNN`, NULL only while Draft. Named for the DOCUMENT rather
    /// than for the family, because the document IS the credit note: the Spanish
    /// label is the specific one and the identifier follows it. This is the one
    /// field that is deliberately NOT the mirror of `return_number`.
    pub credit_note_number: Option<String>,
    /// Copied from the parent sale, so the credit note is self-contained on its
    /// own page.
    pub customer_id: i64,
    /// The confirmed sale this reverses. `RESTRICT`, as on the purchase twin.
    pub sale_id: i64,
    pub status: CustomerReturnStatus,
    /// The day the return is MADE: the goods come in today and the refund leaves
    /// today.
    pub return_date: NaiveDate,
    pub notes: String,
    pub cancel_reason: Option<String>,
    /// Audit actor, as on [`PurchaseReturn`].
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub confirmed_at: Option<chrono::NaiveDateTime>,
    pub cancelled_at: Option<chrono::NaiveDateTime>,
}

/// One line of a credit note: a quantity OF a parent sale line, at that line's
/// price. No `product_id` and no `tax_total`, both for the reasons given on
/// [`PurchaseReturnLine`] — the line names its parent line, and the product and
/// any tax are one read away through it.
///
/// **There is deliberately no `unit_cost` here**, and this is the one place the
/// two families are NOT a clean mirror. What a sale was actually PROFITABLE at
/// needs the cost the goods carried on the day they sold, and `sale_lines` freezes
/// tax but not cost — so a future margin report would find this document unable to
/// answer the question it exists to answer. The app computes no margin today, so
/// nothing needs the figure; the cost snapshot is to be added when a margin report
/// exists, once, rather than paid for speculatively here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerReturnLine {
    pub id: i64,
    pub return_id: i64,
    /// The parent sale line being returned. `RESTRICT`, and `UNIQUE` per return
    /// with this column. Two credit note lines may still carry the SAME product,
    /// because a sale may carry it twice — the key is about lines, not products.
    pub sale_line_id: i64,
    /// Decimal qty > 0, stored as TEXT.
    pub qty: Decimal,
    /// Decimal unit_price >= 0, FROZEN from the parent sale line when this line
    /// was added, for the same reason the tax breakdown is frozen. A return is
    /// always at the sale price, so the form has no price field.
    pub unit_price: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

impl CustomerReturnLine {
    /// The line's money: the returned quantity at the frozen price. Multiplication
    /// only, and no rounding, exactly as on the purchase twin.
    pub fn subtotal(&self) -> Decimal {
        self.qty * self.unit_price
    }
}

/// One refund of a credit note: money going back to the customer, so it is an
/// Expense and a refund the business cannot pay is refused rather than promised.
///
/// **There is deliberately no `receipt_id`**, where `SalePayment` carries one: a
/// customer receipt groups a COLLECTION, and a credit note is a refund. Nothing
/// about a return could be receipt-grouped, so the column would be a second thing
/// to be NULL. Both transaction links are present from the start, as
/// [`PurchaseReturnPayment`]'s are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerReturnPayment {
    pub id: i64,
    pub return_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    /// Decimal amount > 0, stored as TEXT, capped at what the parent sale has
    /// actually collected.
    pub amount: Decimal,
    pub date: NaiveDate,
    /// Finance transaction this refund created (NULL for historical rows).
    pub transaction_id: Option<i64>,
    /// Refund transaction created when THIS CREDIT NOTE was reversed, if it was.
    pub refund_transaction_id: Option<i64>,
    /// Audit actor, as on [`PurchaseReturnPayment`].
    pub created_by: i64,
    pub updated_by: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// Aggregated credit-note view with derived totals (never stored as truth).
///
/// [`PurchaseReturnDetail`]'s twin in every particular, including the two the
/// migrations force: no `tax_total`, because a credit note line freezes no tax,
/// and no `payment_type`, because the parent sale's payment rows are the authority.
/// `paid` is money REFUNDED OUT, so the direction is the purchase return's mirror
/// and the figure itself is the same shape.
#[derive(Debug, Clone, Serialize)]
pub struct CustomerReturnDetail {
    pub customer_return: CustomerReturn,
    pub lines: Vec<CustomerReturnLine>,
    pub payments: Vec<CustomerReturnPayment>,
    /// `sum(line.subtotal())` — the money before tax, which here is all of it.
    pub net_subtotal: Decimal,
    /// The document total: the tax-inclusive figure, and with no frozen tax on a
    /// line the same number as `net_subtotal`.
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

/// Format `YYYY-PRET-NNNNNN` with zero-padded 6-digit sequence. The short form
/// over `PURCH-RET` because the number is read aloud and typed by hand at a
/// counter, and four characters is one fewer pair of hands on a keyboard. `PRET`
/// collides with neither `SALE` nor `PURCH`, the only other consumers of
/// `doc_sequences`.
///
/// `year` is the RETURN's own date year, passed in rather than read from the
/// clock: the same reason the other families take it is an argument is that a
/// document backdated into a past year must consume that year's counter, not
/// this one's.
pub fn format_purchase_return_number(year: i32, seq: i64) -> String {
    format!("{year}-PRET-{seq:06}")
}

/// Format `YYYY-SRET-NNNNNN` with zero-padded 6-digit sequence, the credit note's
/// twin of [`format_purchase_return_number`] — same width, same reason for it,
/// and the same rule that `year` is the return's own date year.
pub fn format_customer_return_number(year: i32, seq: i64) -> String {
    format!("{year}-SRET-{seq:06}")
}

// ---------------------------------------------------------------------------
// Party ledger — one signed entry table per party
// (odd/tasks/party-ledger.md, decisions 1, 3, 7)
// ---------------------------------------------------------------------------

/// Whose ledger an entry belongs to. The value pairs with `party_id`, which
/// carries no foreign key precisely because it means one of two tables
/// depending on this field (migration 42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum PartyType {
    Customer,
    Supplier,
}

impl std::fmt::Display for PartyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Customer => write!(f, "Customer"),
            Self::Supplier => write!(f, "Supplier"),
        }
    }
}

impl std::str::FromStr for PartyType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "customer" => Ok(Self::Customer),
            "supplier" => Ok(Self::Supplier),
            _ => Err(format!("invalid party type: {s}")),
        }
    }
}

/// What happened, as the journal records it. The five events of decision 1's
/// table; a row's `kind` and its stored SIGN together are the whole statement
/// of that rule, so a reader never has to re-derive a direction from context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum PartyEntryKind {
    Charge,
    Payment,
    Return,
    Refund,
    Cancel,
}

impl PartyEntryKind {
    /// THE SIGN RULE of decision 1, in one place so no write path can hold a
    /// second opinion about it:
    ///
    /// | event | kind | sign |
    /// |---|---|---|
    /// | document confirmed (cash or credit) | `Charge` | `+total` |
    /// | cash settled against the document | `Payment` | `−amount` |
    /// | goods returned / credit note | `Return` | `−total` |
    /// | cash handed back to the party | `Refund` | `+amount` |
    /// | whole document annulled | `Cancel` | `−total` |
    ///
    /// The argument is the magnitude as the document states it — a total or a
    /// payment amount, i.e. a figure that is already non-negative on a
    /// well-formed document. The returned value is what gets STORED: the sign
    /// lives in the row, and the read side is one checked sum over `amount`,
    /// never a sign function of its own.
    ///
    /// Worked examples the feature document pins: a customer who owes 200 and
    /// pays 250 folds to `−50` (a credit); a 100 credit note returned in full
    /// before any payment folds to `+100 −100 = 0`.
    pub fn signed_amount(self, magnitude: Decimal) -> Decimal {
        match self {
            Self::Charge | Self::Refund => magnitude,
            Self::Payment | Self::Return | Self::Cancel => -magnitude,
        }
    }
}

impl std::fmt::Display for PartyEntryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Charge => write!(f, "Charge"),
            Self::Payment => write!(f, "Payment"),
            Self::Return => write!(f, "Return"),
            Self::Refund => write!(f, "Refund"),
            Self::Cancel => write!(f, "Cancel"),
        }
    }
}

impl std::str::FromStr for PartyEntryKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "charge" => Ok(Self::Charge),
            "payment" => Ok(Self::Payment),
            "return" => Ok(Self::Return),
            "refund" => Ok(Self::Refund),
            "cancel" => Ok(Self::Cancel),
            _ => Err(format!("invalid party entry kind: {s}")),
        }
    }
}

/// The family of the document an entry points at. The pair (kind, id) is the
/// reference — four families, one column set, so it cannot be a foreign key
/// (migration 42's header says why that is a property of SQLite and not a
/// shortcut).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum PartyDocumentKind {
    Sale,
    Purchase,
    CustomerReturn,
    PurchaseReturn,
}

impl std::fmt::Display for PartyDocumentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sale => write!(f, "Sale"),
            Self::Purchase => write!(f, "Purchase"),
            Self::CustomerReturn => write!(f, "CustomerReturn"),
            Self::PurchaseReturn => write!(f, "PurchaseReturn"),
        }
    }
}

impl std::str::FromStr for PartyDocumentKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "sale" => Ok(Self::Sale),
            "purchase" => Ok(Self::Purchase),
            "customerreturn" => Ok(Self::CustomerReturn),
            "purchasereturn" => Ok(Self::PurchaseReturn),
            _ => Err(format!("invalid party document kind: {s}")),
        }
    }
}

/// One stored ledger row.
///
/// `amount` is the SIGNED figure — positive is an outstanding obligation,
/// negative is a saldo a favor (see [`PartyEntryKind::signed_amount`]) — and it
/// is what every balance folds, so nothing downstream re-applies a sign.
/// `entry_date` is the event's date as its document wrote it.
///
/// The journal is append-only (decisions 3 and 10): a cancel appends its own
/// entries rather than editing the ones that exist, and the schema itself
/// refuses every `UPDATE` and `DELETE` on the table, so there is no audit
/// stamp beyond `created_by`/`created_at` to drift from what happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartyLedgerEntry {
    pub id: i64,
    pub party_type: PartyType,
    pub party_id: i64,
    pub kind: PartyEntryKind,
    /// Signed Decimal, stored as TEXT. Never `f32`/`f64`.
    pub amount: Decimal,
    pub document_kind: PartyDocumentKind,
    pub document_id: i64,
    pub entry_date: NaiveDate,
    /// The document number, opaque: the ledger never parses it.
    pub reference: Option<String>,
    pub created_by: i64,
    pub created_at: chrono::NaiveDateTime,
}

/// The write shape of [`PartyLedgerEntry`], without the row's identity or its
/// DB-stamped `created_at`.
///
/// `amount` arrives already signed — the caller states the magnitude through
/// [`PartyEntryKind::signed_amount`] — so the column is written exactly once,
/// here, and there is no second place where a direction could be applied.
#[derive(Debug, Clone)]
pub struct NewPartyLedgerEntry {
    pub party_type: PartyType,
    pub party_id: i64,
    pub kind: PartyEntryKind,
    /// Signed Decimal, stored as TEXT.
    pub amount: Decimal,
    pub document_kind: PartyDocumentKind,
    pub document_id: i64,
    pub entry_date: NaiveDate,
    pub reference: Option<String>,
    pub created_by: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL/filter tokens are unique across every family (the property
    /// that lets the route resolve a token back to one family) and never
    /// empty.
    #[test]
    fn document_kind_tokens_are_unique_and_non_empty() {
        let mut seen: Vec<&str> = Vec::new();
        for kind in DocumentKind::ALL {
            let token = kind.token();
            assert!(!token.is_empty(), "{kind:?} has an empty token");
            assert!(!seen.contains(&token), "token {token:?} is used twice");
            seen.push(token);
        }
    }

    /// The group vocabulary round-trips the same way.
    #[test]
    fn document_group_tokens_round_trip_and_reject_unknowns() {
        for group in DocumentGroup::ALL {
            assert_eq!(DocumentGroup::parse(group.token()), Some(*group));
        }
        assert_eq!(DocumentGroup::parse(""), None);
        assert_eq!(DocumentGroup::parse("nope"), None);
    }

    /// `ALL` covers every variant exactly once (a drift here would silently
    /// drop a family from every "parse any token" loop).
    #[test]
    fn document_kind_all_covers_every_variant_exactly_once() {
        let all = DocumentKind::ALL;
        assert_eq!(all.len(), 6);
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// The four groups cover the six families: the union of the four
    /// `kinds()` lists covers every family exactly once, and Payments is a
    /// deliberate wide filter over three families while Stock is the narrow
    /// one. `kinds()` is the filter expansion, so the partition is what makes
    /// it total and duplicate-free — the property the feed's row list
    /// depends on.
    #[test]
    fn document_group_kinds_partition_every_family() {
        // The four groups partition the six families: no family is listed
        // under two filter options, every group names a non-empty list, and
        // the union of the four covers every family.
        let mut covered: Vec<DocumentKind> = Vec::new();
        for group in DocumentGroup::ALL {
            assert!(!group.kinds().is_empty());
            for kind in group.kinds() {
                assert!(
                    !covered.contains(kind),
                    "{kind:?} is listed under two groups"
                );
                covered.push(*kind);
            }
        }
        covered.sort();
        let mut all = DocumentKind::ALL.to_vec();
        all.sort();
        assert_eq!(covered, all);

        // The filter vocabulary: only payments is wide, because the three
        // payment families are one option for the operator; every other
        // group is exactly its own document.
        assert_eq!(DocumentGroup::Sales.kinds(), &[DocumentKind::Sale]);
        assert_eq!(DocumentGroup::Purchases.kinds(), &[DocumentKind::Purchase]);
        assert_eq!(DocumentGroup::Stock.kinds(), &[DocumentKind::StockMovement]);
        assert_eq!(
            DocumentGroup::Payments.kinds(),
            &[
                DocumentKind::SalePayment,
                DocumentKind::PurchasePayment,
                DocumentKind::Receipt,
            ]
        );
    }

    /// The tokens round-trip through `parse`: every family parses back from
    /// its own token, and an unknown or empty token names no family. The
    /// drawer route's `{kind}` segment depends on this exact contract.
    #[test]
    fn document_kind_parse_round_trips_and_rejects_unknowns() {
        for kind in DocumentKind::ALL {
            assert_eq!(DocumentKind::parse(kind.token()), Some(*kind));
        }
        assert_eq!(DocumentKind::parse(""), None);
        assert_eq!(DocumentKind::parse("nope"), None);
    }

    /// The read-code mapping: each family answers the code its OWNING tier
    /// reads with — the sale document and its payment by `sales.read`, the
    /// purchase document and its payment by `purchases.read`, movements by
    /// `inventory.read`, receipts by `customers.read`. The page's narrowing
    /// and the drawer route must agree with this single mapping, so a drift
    /// here would silently open a family under another tier's code.
    #[test]
    fn document_kind_read_code_maps_every_family_to_its_owning_tier() {
        assert_eq!(DocumentKind::Sale.read_code(), "sales.read");
        assert_eq!(DocumentKind::SalePayment.read_code(), "sales.read");
        assert_eq!(DocumentKind::Purchase.read_code(), "purchases.read");
        assert_eq!(DocumentKind::PurchasePayment.read_code(), "purchases.read");
        assert_eq!(DocumentKind::StockMovement.read_code(), "inventory.read");
        assert_eq!(DocumentKind::Receipt.read_code(), "customers.read");
    }

    /// `query(limit)` carries the bounds through unchanged; the family list is
    /// deliberately not part of the per-family query.
    #[test]
    fn document_filter_query_carries_bounds_and_limit() {
        let filter = DocumentFilter {
            kinds: vec![DocumentKind::Sale],
            actor_ids: Some(vec![7, 9]),
            from: NaiveDate::from_ymd_opt(2024, 1, 1),
            to: NaiveDate::from_ymd_opt(2024, 1, 31),
            search: Some("perez".into()),
        };
        let query = filter.query(50);
        assert_eq!(query.actor_ids.as_deref(), Some(&[7i64, 9][..]));
        assert_eq!(query.from, NaiveDate::from_ymd_opt(2024, 1, 1));
        assert_eq!(query.to, NaiveDate::from_ymd_opt(2024, 1, 31));
        assert_eq!(query.search.as_deref(), Some("perez"));
        assert_eq!(query.limit, 50);

        // An empty filter yields an empty query and the given cap.
        let empty = DocumentFilter::default().query(DOCUMENTS_PAGE_LIMIT);
        assert!(empty.actor_ids.is_none());
        assert!(empty.from.is_none());
        assert!(empty.to.is_none());
        assert!(empty.search.is_none());
        assert_eq!(empty.limit, DOCUMENTS_PAGE_LIMIT);
    }

    // -- money display (product-markup T7) ------------------------------------

    fn dec(s: &str) -> Decimal {
        std::str::FromStr::from_str(s).unwrap()
    }

    /// At-or-below-2 scale is normalised UP to exactly two decimals: the same
    /// list never shows `$100` and `$100.00` side by side.
    #[test]
    fn money_display_scales_short_values_up_to_two_decimals() {
        assert_eq!(money_display(dec("7.5")), "7.50");
        assert_eq!(money_display(dec("100")), "100.00");
        assert_eq!(money_display(dec("10.00")), "10.00");
    }

    /// The never-lie guard: a value stored with MORE than two decimals prints
    /// exactly as stored — rounding for display would misstate the price the
    /// customer is charged.
    #[test]
    fn money_display_never_rounds_finer_than_two_decimals() {
        assert_eq!(money_display(dec("7.777")), "7.777");
    }

    // -- purchase returns and credit notes (odd/tasks/purchase-returns-and-
    //    credit-notes.md) --------------------------------------------------
    //
    // The model layer only: these pin the number format, the status dialect and
    // the shape of the two mirrored families. Every rule about what a return
    // DOES lives in a service unit, and the columns are the migrations', so
    // nothing here needs a database.

    /// A fixed instant, so a struct can be built without a clock.
    fn return_ts() -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 4)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap()
    }

    fn purchase_return(
        status: PurchaseReturnStatus,
        return_number: Option<&str>,
    ) -> PurchaseReturn {
        PurchaseReturn {
            id: 2,
            return_number: return_number.map(str::to_string),
            supplier_id: 4,
            purchase_id: 9,
            status,
            return_date: NaiveDate::from_ymd_opt(2026, 3, 4).unwrap(),
            notes: String::new(),
            cancel_reason: None,
            created_by: 1,
            updated_by: None,
            created_at: return_ts(),
            updated_at: return_ts(),
            confirmed_at: None,
            cancelled_at: None,
        }
    }

    fn purchase_return_line(qty: &str, unit_cost: &str) -> PurchaseReturnLine {
        PurchaseReturnLine {
            id: 5,
            return_id: 2,
            purchase_line_id: 11,
            qty: dec(qty),
            unit_cost: dec(unit_cost),
            created_at: return_ts(),
        }
    }

    fn purchase_return_payment(
        transaction_id: Option<i64>,
        refund_transaction_id: Option<i64>,
    ) -> PurchaseReturnPayment {
        PurchaseReturnPayment {
            id: 7,
            return_id: 2,
            account_id: 3,
            method_id: 6,
            amount: dec("7.50"),
            date: NaiveDate::from_ymd_opt(2026, 3, 4).unwrap(),
            transaction_id,
            refund_transaction_id,
            created_by: 1,
            updated_by: None,
            created_at: return_ts(),
            updated_at: return_ts(),
        }
    }

    fn customer_return(
        status: CustomerReturnStatus,
        credit_note_number: Option<&str>,
    ) -> CustomerReturn {
        CustomerReturn {
            id: 2,
            credit_note_number: credit_note_number.map(str::to_string),
            customer_id: 4,
            sale_id: 9,
            status,
            return_date: NaiveDate::from_ymd_opt(2026, 3, 4).unwrap(),
            notes: String::new(),
            cancel_reason: None,
            created_by: 1,
            updated_by: None,
            created_at: return_ts(),
            updated_at: return_ts(),
            confirmed_at: None,
            cancelled_at: None,
        }
    }

    fn customer_return_line(qty: &str, unit_price: &str) -> CustomerReturnLine {
        CustomerReturnLine {
            id: 5,
            return_id: 2,
            sale_line_id: 11,
            qty: dec(qty),
            unit_price: dec(unit_price),
            created_at: return_ts(),
        }
    }

    fn customer_return_payment(
        transaction_id: Option<i64>,
        refund_transaction_id: Option<i64>,
    ) -> CustomerReturnPayment {
        CustomerReturnPayment {
            id: 7,
            return_id: 2,
            account_id: 3,
            method_id: 6,
            amount: dec("7.50"),
            date: NaiveDate::from_ymd_opt(2026, 3, 4).unwrap(),
            transaction_id,
            refund_transaction_id,
            created_by: 1,
            updated_by: None,
            created_at: return_ts(),
            updated_at: return_ts(),
        }
    }

    /// The exact rendered shape is asserted rather than derived, because the
    /// number is read aloud and typed by hand at a counter: a dropped zero pad
    /// or a long prefix is a typo the operator makes every time. The width is a
    /// MINIMUM, not a ceiling — a seventh digit is printed, never truncated.
    #[test]
    fn format_purchase_return_number_pads_the_sequence_to_six_digits_under_the_short_prefix() {
        assert_eq!(format_purchase_return_number(2026, 1), "2026-PRET-000001");
        assert_eq!(format_purchase_return_number(2026, 42), "2026-PRET-000042");
        assert_eq!(
            format_purchase_return_number(2026, 1_234_567),
            "2026-PRET-1234567"
        );
        // Sixteen characters, the same as a sale number and one shorter than a
        // purchase number — the whole reason the short form was chosen. (The
        // feature document calls these "15 characters"; a four-character prefix
        // and a six-digit sequence make sixteen, and the relative claim is the
        // one that decided it.)
        assert_eq!(format_purchase_return_number(2026, 1).len(), 16);
        assert_eq!(format_sale_number(2026, 1).len(), 16);
        assert_eq!(format_purchase_number(2026, 1).len(), 17);
    }

    /// The customer return's twin, asserted the same way: the two families are
    /// mirrored, and a formatter that only one of them proved is a formatter one
    /// of them got wrong.
    #[test]
    fn format_customer_return_number_pads_the_sequence_to_six_digits_under_the_short_prefix() {
        assert_eq!(format_customer_return_number(2026, 1), "2026-SRET-000001");
        assert_eq!(format_customer_return_number(2026, 42), "2026-SRET-000042");
        assert_eq!(
            format_customer_return_number(2026, 1_234_567),
            "2026-SRET-1234567"
        );
        assert_eq!(format_customer_return_number(2026, 1).len(), 16);
        assert_eq!(
            format_customer_return_number(2026, 1).len(),
            format_purchase_return_number(2026, 1).len()
        );
    }

    /// The prefixes are distinct, so no two families can render the same number
    /// for the same year and sequence. `doc_sequences` keys on a free-form
    /// `doc_type` with no CHECK, so nothing at the database would catch a
    /// collision between two consumers that picked the same literal — this is
    /// the only place that does.
    #[test]
    fn the_two_return_number_prefixes_collide_with_no_existing_document_number() {
        let rendered = [
            format_sale_number(2026, 7),
            format_purchase_number(2026, 7),
            format_purchase_return_number(2026, 7),
            format_customer_return_number(2026, 7),
        ];
        for (i, a) in rendered.iter().enumerate() {
            for b in &rendered[i + 1..] {
                assert_ne!(a, b, "two families render the same document number: {a}");
            }
        }

        // And the middle segment alone, so a number read aloud cannot be
        // mistaken for one of the other three even before the sequence is said.
        let prefixes: Vec<&str> = rendered
            .iter()
            .map(|n| n.split('-').nth(1).unwrap())
            .collect();
        assert_eq!(prefixes, vec!["SALE", "PURCH", "PRET", "SRET"]);
    }

    /// `Display` is what the repository binds into the `status` column, so it
    /// must write exactly the three words the migration's CHECK accepts. A
    /// fourth variant or a lowercase word would be refused by the database at
    /// the worst possible moment: on confirm.
    #[test]
    fn purchase_return_status_display_writes_exactly_the_three_words_the_check_accepts() {
        assert_eq!(PurchaseReturnStatus::Draft.to_string(), "Draft");
        assert_eq!(PurchaseReturnStatus::Confirmed.to_string(), "Confirmed");
        assert_eq!(PurchaseReturnStatus::Cancelled.to_string(), "Cancelled");
    }

    /// The customer return's twin of the same contract.
    #[test]
    fn customer_return_status_display_writes_exactly_the_three_words_the_check_accepts() {
        assert_eq!(CustomerReturnStatus::Draft.to_string(), "Draft");
        assert_eq!(CustomerReturnStatus::Confirmed.to_string(), "Confirmed");
        assert_eq!(CustomerReturnStatus::Cancelled.to_string(), "Cancelled");
    }

    /// `FromStr` is the read direction of the same column, and it is the tolerant
    /// one: it folds case and takes BOTH spellings of cancelled, because that is
    /// what `SaleStatus` and `PurchaseStatus` already do. A new enum that parsed
    /// only `cancelled` would be a second dialect for the same three values, and
    /// the row written by one reader would fail the next.
    #[test]
    fn purchase_return_status_from_str_folds_case_and_accepts_both_spellings_of_cancelled() {
        for (input, expected) in [
            ("Draft", PurchaseReturnStatus::Draft),
            ("draft", PurchaseReturnStatus::Draft),
            ("DRAFT", PurchaseReturnStatus::Draft),
            ("Confirmed", PurchaseReturnStatus::Confirmed),
            ("confirmed", PurchaseReturnStatus::Confirmed),
            ("Cancelled", PurchaseReturnStatus::Cancelled),
            ("cancelled", PurchaseReturnStatus::Cancelled),
            ("canceled", PurchaseReturnStatus::Cancelled),
            ("CANCELED", PurchaseReturnStatus::Cancelled),
        ] {
            assert_eq!(
                input.parse::<PurchaseReturnStatus>().unwrap(),
                expected,
                "parsing {input:?}"
            );
        }

        // What `Display` writes is exactly what `FromStr` reads back: that is the
        // whole contract with the CHECK.
        for status in [
            PurchaseReturnStatus::Draft,
            PurchaseReturnStatus::Confirmed,
            PurchaseReturnStatus::Cancelled,
        ] {
            assert_eq!(
                status.to_string().parse::<PurchaseReturnStatus>().unwrap(),
                status
            );
        }

        // Anything else refuses and names the family, so a read of an unknown
        // value cannot be mistaken for a Draft.
        let err = "archived".parse::<PurchaseReturnStatus>().unwrap_err();
        assert_eq!(err, "invalid purchase return status: archived");
    }

    /// The customer return's twin, including its own error wording — the two
    /// families are separate types and a reader that conflates them in a log is
    /// being told the wrong table.
    #[test]
    fn customer_return_status_from_str_folds_case_and_accepts_both_spellings_of_cancelled() {
        for (input, expected) in [
            ("Draft", CustomerReturnStatus::Draft),
            ("draft", CustomerReturnStatus::Draft),
            ("DRAFT", CustomerReturnStatus::Draft),
            ("Confirmed", CustomerReturnStatus::Confirmed),
            ("confirmed", CustomerReturnStatus::Confirmed),
            ("Cancelled", CustomerReturnStatus::Cancelled),
            ("cancelled", CustomerReturnStatus::Cancelled),
            ("canceled", CustomerReturnStatus::Cancelled),
            ("CANCELED", CustomerReturnStatus::Cancelled),
        ] {
            assert_eq!(
                input.parse::<CustomerReturnStatus>().unwrap(),
                expected,
                "parsing {input:?}"
            );
        }

        for status in [
            CustomerReturnStatus::Draft,
            CustomerReturnStatus::Confirmed,
            CustomerReturnStatus::Cancelled,
        ] {
            assert_eq!(
                status.to_string().parse::<CustomerReturnStatus>().unwrap(),
                status
            );
        }

        let err = "archived".parse::<CustomerReturnStatus>().unwrap_err();
        assert_eq!(err, "invalid customer return status: archived");
    }

    /// A return is partial and its price is the parent's, frozen when the line
    /// was added — so the amount is the RETURNED quantity at that price, not the
    /// parent line's full quantity, and the multiplication is not rounded: a
    /// stored cost finer than two decimals is a fact about the purchase, and
    /// rounding it here would misstate what the supplier owes back.
    #[test]
    fn purchase_return_line_totals_the_returned_quantity_at_the_frozen_unit_cost() {
        let line = purchase_return_line("3", "2.50");
        assert_eq!(line.subtotal(), dec("7.50"));

        // Not the parent line's whole quantity: sending 3 of 10 back is 7.50,
        // and the 7 that stay are not this document's money.
        assert_ne!(line.subtotal(), dec("25.00"));

        // The finer-than-two-decimals case, the same never-lie rule the money
        // display follows.
        assert_eq!(purchase_return_line("2", "2.505").subtotal(), dec("5.010"));
        assert_eq!(purchase_return_line("0", "2.50").subtotal(), dec("0"));
    }

    /// The credit note's twin, at the frozen SALE price. Same arithmetic, and
    /// same reason there is nothing on the line that could price it differently.
    #[test]
    fn customer_return_line_totals_the_returned_quantity_at_the_frozen_unit_price() {
        let line = customer_return_line("3", "2.50");
        assert_eq!(line.subtotal(), dec("7.50"));

        assert_ne!(line.subtotal(), dec("25.00"));
        assert_eq!(customer_return_line("2", "2.505").subtotal(), dec("5.010"));
        assert_eq!(customer_return_line("0", "2.50").subtotal(), dec("0"));
    }

    /// The detail is the document plus its children plus the money a record page
    /// needs, and it carries no `payment_type`: the flag belongs to the parent
    /// purchase, and a second copy here could disagree with the payment rows it
    /// summarizes. Nothing can read it off this struct, which is the point.
    #[test]
    fn purchase_return_detail_carries_the_document_its_children_and_a_total_with_no_tax_parted_out()
    {
        let draft = PurchaseReturnDetail {
            purchase_return: purchase_return(PurchaseReturnStatus::Draft, None),
            lines: vec![purchase_return_line("3", "2.50")],
            payments: vec![],
            net_subtotal: dec("7.50"),
            total: dec("7.50"),
            paid: dec("0"),
            due: dec("7.50"),
            payment_status: PaymentStatus::Unpaid,
        };

        // A Draft carries no number, exactly as `purchase_number` does: the
        // number is the last thing confirm assigns.
        assert_eq!(draft.purchase_return.return_number, None);
        assert_eq!(draft.purchase_return.purchase_id, 9);
        assert_eq!(draft.lines.len(), 1);
        assert_eq!(draft.payments.len(), 0);

        // A return line freezes no tax, so there is nothing to separate: net and
        // total are the same figure, and both are the line's own subtotal.
        assert_eq!(draft.net_subtotal, draft.total);
        assert_eq!(draft.total, draft.lines[0].subtotal());
        assert_eq!(draft.due, draft.total - draft.paid);

        let confirmed = PurchaseReturnDetail {
            purchase_return: purchase_return(
                PurchaseReturnStatus::Confirmed,
                Some("2026-PRET-000001"),
            ),
            payments: vec![purchase_return_payment(Some(31), None)],
            paid: dec("7.50"),
            due: dec("0"),
            payment_status: PaymentStatus::Paid,
            ..draft
        };
        assert_eq!(
            confirmed.purchase_return.return_number.as_deref(),
            Some("2026-PRET-000001")
        );
        assert_eq!(confirmed.payments[0].transaction_id, Some(31));
        assert_eq!(confirmed.due, Decimal::ZERO);
    }

    /// The credit note's twin. `credit_note_number` is named for the DOCUMENT
    /// rather than the family, so the field name is deliberately not the mirror
    /// of `return_number` — the Spanish label is the specific one and the
    /// identifier follows it.
    #[test]
    fn customer_return_detail_carries_the_document_its_children_and_a_total_with_no_tax_parted_out()
    {
        let draft = CustomerReturnDetail {
            customer_return: customer_return(CustomerReturnStatus::Draft, None),
            lines: vec![customer_return_line("3", "2.50")],
            payments: vec![],
            net_subtotal: dec("7.50"),
            total: dec("7.50"),
            paid: dec("0"),
            due: dec("7.50"),
            payment_status: PaymentStatus::Unpaid,
        };

        assert_eq!(draft.customer_return.credit_note_number, None);
        assert_eq!(draft.customer_return.sale_id, 9);
        assert_eq!(draft.lines.len(), 1);
        assert_eq!(draft.payments.len(), 0);
        assert_eq!(draft.net_subtotal, draft.total);
        assert_eq!(draft.total, draft.lines[0].subtotal());
        assert_eq!(draft.due, draft.total - draft.paid);

        let confirmed = CustomerReturnDetail {
            customer_return: customer_return(
                CustomerReturnStatus::Confirmed,
                Some("2026-SRET-000001"),
            ),
            payments: vec![customer_return_payment(Some(31), None)],
            paid: dec("7.50"),
            due: dec("0"),
            payment_status: PaymentStatus::Paid,
            ..draft
        };
        assert_eq!(
            confirmed.customer_return.credit_note_number.as_deref(),
            Some("2026-SRET-000001")
        );
        assert_eq!(confirmed.payments[0].transaction_id, Some(31));
        assert_eq!(confirmed.due, Decimal::ZERO);
    }

    /// Both link columns are born in the CREATE, so all three states are
    /// representable: a payment with only its own movement, one that has been
    /// refunded, and one that has been both — which is what reversing a return
    /// that was itself confirmed produces. A single nullable column could not
    /// hold the third, which is why migration 19 put both in the same table.
    ///
    /// There is no `receipt_id` on either: a return is a refund, and a customer
    /// receipt groups a COLLECTION. Nothing on a return could carry one.
    #[test]
    fn a_return_payment_keeps_its_own_transaction_link_and_its_refund_link_independently() {
        let paid = purchase_return_payment(Some(31), None);
        assert_eq!(paid.transaction_id, Some(31));
        assert_eq!(paid.refund_transaction_id, None);

        let refunded = purchase_return_payment(Some(31), Some(44));
        assert_eq!(refunded.transaction_id, Some(31));
        assert_eq!(refunded.refund_transaction_id, Some(44));

        let credit = customer_return_payment(Some(52), Some(53));
        assert_eq!(credit.transaction_id, Some(52));
        assert_eq!(credit.refund_transaction_id, Some(53));
    }
}
