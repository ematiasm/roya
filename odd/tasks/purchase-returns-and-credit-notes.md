# Purchase returns and credit notes

## Status

Design. No migration, no route, no template written yet. Nothing in the tree has
been changed by this document.

## Objective

Two new document families that reverse part or all of a confirmed purchase or sale:

- **Devolución de compra** — the business returns goods **to a supplier**. Stock
  goes out, money comes in.
- **Nota de crédito** — a customer returns goods **to the business**. Stock comes
  in, money goes out.

Both are real documents with their own number, their own status, their own lines
and their own payments. Neither is a stock movement with a refund bolted on: today
the only way to reverse a document is to cancel it, which is all-or-nothing.

## Decisions

1. **Decided 2026-09-30 — a return is always at the parent's price. Quantities
   vary; prices never do.** The return form has no price field, and that is not a
   UI restriction: a return at a different price has no defined answer, because
   `product_supplier_costs` holds one price per (product, supplier) and the
   supplier's cost history is the fact being recorded. A return is the absence of
   goods the business still owns, not a renegotiation.

2. **Decided 2026-09-30 — a return does NOT write the cost satellite.** Only a
   confirmed purchase changes a product's cost, because only a purchase sets a
   price. A return at the purchase price changes no price, so `current_cost`,
   `current_cost_date`, `previous_cost` and `previous_cost_date` are all untouched
   — and the derived price-change alert does not fire, which is correct: the
   supplier's price did not move.

   This is what makes the satellite repairable. With the rule above, the satellite
   is a fold over confirmed purchase lines rather than a set of rows that must be
   kept in sync, so a wrong row can be recomputed instead of reconstructed by
   hand. See the still-open section for what closing that would take.

3. **Decided 2026-09-30 — the movement keeps its existing name; only the document
   is new.** `MovementReason::PurchaseReturn` and `MovementReason::SaleReturn`
   already exist and are already valid in the database's CHECK (migrations 11, 18
   and 31 extended it). They describe a physical event — goods out, goods in — and
   are accurate. Renaming a stored reason to match a document name would be a
   migration for no gain, and the stock reason is the one thing a reader of a
   movement row can trust without opening anything.

4. **Decided 2026-09-30 — the new document is `CustomerReturn`, the Spanish label
   is "Nota de crédito", and the English label is "Customer return".** The
   identifier is English and deliberately differs from `SaleReturn` so the document
   and the movement can never be confused in code either. The Spanish term is more
   specific than the English one, which is normal: both catalogs describe the same
   document, each in the word its own language uses.

   **Superseded in part on 2026-10-01:** the identifier and the Spanish label above
   stand, but the English LABEL was reversed by decision 5 and now reads "Credit
   note". This paragraph is left as written on 2026-09-30 because the reasoning it
   records — that the identifier deliberately differs from `SaleReturn` — is still
   what the code does, and because a decision that is quietly rewritten is a
   decision nobody can audit. Read the label claim here through decision 5.

5. **Decided 2026-09-30, REVERSED 2026-10-01 — the English side says "Credit note",
   and the reversal is the decision.** Read this as history, not as a finished
   rule with a typo in it:

   **As first decided (2026-09-30), the English side stayed plain.** The app
   already chose "Cancel sale" for "Anular venta" and "refunded" for
   "reembolsados" rather than "Annul" or "reimbursed". "Credit note" was held to be
   real accounting English but jargon, and this app's English does not use jargon.
   "Customer return" was called the word a shopkeeper would use, and the button on
   the sale's record page was labelled **"Take goods back"**.

   **Reversed 2026-10-01 by the domain owner. English says "Credit note".**

   The original reasoning was backwards, and the mistake was a category error
   rather than a matter of taste: **"Take goods back" names a physical action, and
   a credit note names a document.** What appears on screen — on the sale's record
   page, in the list, in the nav — is a document, not a warehouse movement. The
   label was asked to do a job only a document's name can do.

   The corroborating evidence was in the tree the whole time: **the Spanish was
   already "Nota de crédito"**, and the English was the outlier rather than the
   Spanish. Decision 4 stands and is no longer in tension with this one — both
   catalogs now name the same document, each in the word its own language uses,
   which is what decision 4 always claimed was normal.

   What the reversal touches, and what it deliberately does not:

   - The **action button and its block heading** read "Credit note". That one
     `MessageKey` renders in three places in `sale_detail.html` (`data-action`,
     `<h2>`, submit button); a noun passes all three.
   - The **page title, the nav entry and the list count** read "Credit notes".
     The count travels with the title on purpose: the list header renders
     `title • count` on one line, so renaming one and not the other puts two
     different names for the same document in a single heading.
   - The **purchase side is not touched.** "Devolución de compra" is a different
     document with a different Spanish name already, and it is not in question.
   - `MovementReason::SaleReturn` is still `SaleReturn` — decision 3, unchanged. A
     movement row records a physical event and should be readable as one.

   **This decision is executable, not aspirational.** Two tests pin it:
   `the_customer_returns_index_renders_the_localized_title_in_both_catalogs`
   (list side) and
   `a_confirmed_sale_record_offers_issue_credit_note_with_this_sale_id_already_filled`
   (action side). Reversing it again is one string plus those two assertions —
   which is the point of stating it this way rather than leaving it to taste.

6. **Decided 2026-09-30 — two mirrored tables, not one table with a discriminator.**
   The repository already mirrors every domain: `sales`/`purchases`,
   `sale_lines`/`purchase_lines`, `sale_payments`/`purchase_payments`,
   `sale_line_taxes`/`purchase_line_taxes`, `set_confirmed` on both repositories.
   A third mirror is idiomatic here; a unified table with a type column would be
   the first polymorphism in the persistence layer and would make every read a
   branch.

## The two families, side by side

|  | Purchase return | Customer return |
|---|---|---|
| Actor | the business → supplier | customer → business |
| Stock | **Out**, reason `Purchase-return` | **In**, reason `Sale-return` |
| Money | **Income** per originating account | **Expense** per originating account |
| Overdraft guard | never fires (Income) | **fires** (Expense), as it does on cancel |
| Parent | a **Confirmed** purchase | a **Confirmed** sale |
| Frozen per line | the parent line's `unit_cost` | the parent line's `unit_price` |
| Writes the satellite | no | no |
| Partial | yes, per line quantity | yes, per line quantity |
| Its own number | yes | yes |

Everything on that table except the three marked rows is identical. That is the
argument for building one engine and configuring it twice.

## Why the two numbers are frozen on the line

The return line stores the parent's `unit_cost` (or `unit_price`) at the moment the
line is added, and that stored figure is what the document totals and the refunds
are computed from.

Two reasons, and the first is the precedent: **the tax snapshot** (migration 39)
freezes `tax_code`, `tax_name`, `rate` and `amount` on every line precisely so a
re-rated or deactivated tax cannot rewrite confirmed history. A return's price is
the same kind of fact. The second is that the parent's line cannot be edited after
confirm anyway, so reading it at confirm would be safe — but the number the
operator saw when they added the line should be the number the document carries,
for the same reason the tax one does.

## The refund cap, and the limitation it hides

A return refunds **at most what the parent document has actually collected.**

The rule exists because a purchase or sale can be confirmed and only partly paid.
Refunding more than came in would leave a negative receivable or a negative
payable, and the app has no credit-balance concept.

**This is a real limitation and it is v1's answer, not a solution.** A shop that
buys on credit, then returns goods before paying, has a legitimate case the cap
refuses. The honest statement: the app has no notion of a credit owed by a
supplier, so that case has nowhere to live yet.

7. **Decided 2026-09-30 — a return does not store `payment_type`; it inherits the
   parent's.** `purchases` and `sales` both carry a `payment_type` column, and the
   refund cap reads it. But a return's refunds are determined entirely by the
   parent's payment rows — which account each payment came from, how much was
   collected, when — so storing a second copy of the flag on the return would be a
   value that could disagree with the rows it summarizes. `purchase_returns` and
   `customer_returns` therefore have no such column, and the service reads the
   parent's.

8. **Decided 2026-09-30 — both payment link columns are in the CREATE, not deferred.**
   `purchase_payments` and `sale_payments` were **born** with `transaction_id` and
   `refund_transaction_id` together (migration 19), and were only given `updated_at`
   afterwards (migration 36). The alternative — create without the refund link and
   `ALTER TABLE ADD COLUMN` when the annulment path arrives — is exactly the
   rebuild-shaped pain migration 19 avoided. Both go in now, nullable, with their
   two indexes.

9. **Decided 2026-09-30 — `doc_sequences` costs no migration at all.**
   `migrations/20240101000007_create_doc_sequences.sql` is the whole table:

   ```sql
   CREATE TABLE IF NOT EXISTS doc_sequences (
       doc_type TEXT NOT NULL,
       year INTEGER NOT NULL,
       last_number INTEGER NOT NULL DEFAULT 0,
       PRIMARY KEY (doc_type, year)
   );
   ```

   No CHECK, no enumeration, no seed rows. `doc_type` is a free-form `&str` in the
   trait and rows are created lazily by the upsert. Two new consumers are therefore
   **two string literals and two `format_*_number` functions** — a migration that
   touches this table would be a defect.

10. **Decided 2026-09-30 — the new tables carry their audit columns inline.**
    `migrations/20240101000036` is the precedent for a table born with them:

    ```sql
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ```

    No trigger is needed and none is installed: SQLite only allows a constant
    default on `ADD COLUMN`, which is why the legacy tables carry a `'1970-01-01'`
    sentinel plus a `trg_*_set_updated_at` trigger. A new table never has that
    problem. Mutating `updated_at` is the application's job, written into the
    UPDATE — the same convention `sale_repo.rs` and `customer_repo.rs` already use.

## Schema

Mirroring `purchases`/`purchase_lines`/`purchase_payments` exactly.

**`purchase_returns`**

| Column | Notes |
|---|---|
| `id` | `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `return_number` | `TEXT NULL UNIQUE`, `NULL` in Draft and Cancelled-from-Draft, as `purchase_number` is |
| `status` | `TEXT NOT NULL CHECK (status IN ('Draft','Confirmed','Cancelled'))` — same three |
| `supplier_id` | `NOT NULL REFERENCES suppliers(id) ON DELETE RESTRICT` |
| `purchase_id` | `NOT NULL REFERENCES purchases(id) ON DELETE RESTRICT` — history survives |
| `return_date` | `TEXT NOT NULL` — **the day the return is made, not the parent's date** |
| `notes`, `cancel_reason` | as on `purchases` |
| `created_by`, `updated_by`, `created_at`, `updated_at` | inline per decision 10 |
| `confirmed_at`, `cancelled_at` | `TEXT NULL` |

**No `payment_type` column** — decision 7. The parent's payment rows are the
authority on what was collected and to which account.

**`purchase_return_lines`**

| Column | Notes |
|---|---|
| `id` | `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `return_id` | `NOT NULL REFERENCES purchase_returns(id) ON DELETE CASCADE` |
| `purchase_line_id` | `NOT NULL REFERENCES purchase_lines(id) ON DELETE RESTRICT` — the link to the parent's line |
| `qty` | `TEXT NOT NULL` |
| `unit_cost` | `TEXT NOT NULL` — frozen from the parent line |
| `created_at` | as on `purchase_lines` |

`purchase_return_payments` mirrors `purchase_payments` with `return_id`,
`transaction_id` **and** `refund_transaction_id` both `INTEGER NULL REFERENCES
transactions(id) ON DELETE RESTRICT` per decision 8, plus their two indexes.

`customer_returns`, `customer_return_lines` and `customer_return_payments` are the
same three with `sale_id` / `sale_line_id` / `unit_price` and no `supplier_id`.

**`UNIQUE (return_id, purchase_line_id)`** on the lines. One line per parent line:
the return's quantity is a quantity OF that line, and splitting one parent line
into two return lines has no defined answer, for the same reason purchases forbid
a repeated product.

## Document numbers

`doc_sequences` needs **no migration** — decision 9. Two new consumers and two
`format_*_number` functions, matching the existing hard-coded-per-family shape at
`src/models.rs:1433` and `:2128`. The `year` is the **return's own date year**, not
the current year and not the parent's.

**Decided 2026-09-30 — the short form.**

| | |
|---|---|
| Purchase return | `YYYY-PRET-NNNNNN` |
| Customer return | `YYYY-SRET-NNNNNN` |

Short over the long `PURCH-RET` / `SALE-RET` because the number is read aloud and
typed by hand at a counter, and four characters is one fewer pair of hands on a
keyboard. Neither collides with an existing prefix: `SALE` and `PURCH` are the only
consumers today, and `SRET` and `PRET` are distinct from both. The rendered strings
are 15 characters — the same as `YYYY-SALE-NNNNNN` and one shorter than
`YYYY-PURCH-NNNNNN`.

Nothing in the tree measures a number's width, so the mixed prefix lengths are not
a hazard. But a test should assert the exact shape, because the existing families
only assert theirs end to end through `confirm` and there is no shared test.

## What confirm does, and why it is safe now

The order mirrors `confirm` exactly, and it runs inside ONE transaction opened
immediately before the number is taken:

1. `next_number`
2. one stock movement per line, with the family's reason and sign
3. one finance row per refund, `reference` = the return's own number
4. the payment rows
5. `set_confirmed`
6. **no satellite write** — by decision 2

The transaction is not a nicety here. It is what the whole Phase A/B sequence was
for: a return has more steps than a sale (a refund to pay AND a refund to collect)
and a failure part-way through a refund is the same class of residue that used to
leave a Draft reporting itself Paid. Here it leaves a Draft with nothing.

## What already exists that this reuses

Not one of these is new, and naming them is the reason this is a smaller job than it
looks:

- `MovementReason::PurchaseReturn` and `MovementReason::SaleReturn`, already in the
  database CHECK
- The annulment path on both documents, which already reverses stock per line and
  refunds per originating account — `sales.rs` and `purchases.rs`
- `doc_sequences` with per-consumer counters
- The `set_confirmed` Draft predicate and its `refuse_confirm` helper
- The `_in` transaction twins across ten repositories and three service seams
- The `No`↔`In` mirror shape: every pair already exists in the repository
- The tax-snapshot pattern for freezing a per-line fact that later rules could move

## Still open

1. **The credit-balance case.** A return worth more than was collected is refused
   by the cap. The shop that buys on credit and returns before paying has no home
   today. Needs a decision, not a workaround.
2. **The cost snapshot on a customer-return line.** Today the app computes no
   margin, so nothing needs it. The moment it does, "was that sale actually
   profitable" needs the cost the goods carried ON THE DAY THEY SOLD, and
   `sale_lines` freezes tax but not cost. The credit note is exactly when a merchant
   looks backwards. Decision: **do not add it now**; add it when a margin report
   exists, once.
3. **The satellite as a derived fold.** Decision 2 makes it derivable in principle.
   Making it actually derived is a larger change: the eager write would go, the
   `shift_cost`/`refresh_cost_date` pair with it, and `current_cost` would be read
   from the latest confirmed purchase per pair. Worth doing eventually, because a
   derived value can be recomputed and a stored one cannot.
4. **Return of a return.** Whether a credit note can itself be returned, and whether
   a purchase return can be annulled once confirmed. The mirror of the existing
   cancel rules answers most of it; the residue is what happens to the parent's own
   refund when its reversal reverses.
5. **Number format.** The long and short forms above.

## What is explicitly out of scope

- Margin, profit and any report over it
- Any change to the price ladder, the cost reference policy, or the
  `products.cost_price` field — the cost design was reviewed and left as it is
- Changing `MovementReason` names or the stored reason values
- Partial credit on a single return line beyond its quantity
