# Capability: documents

## Purpose
One screen over the documents the shop produces — sales, sale payments, purchases, purchase payments,
stock movements and customer receipts — newest first, with a text search and filters by type, acting
user and date range. It is a read-only INDEX: it owns no table, writes nothing, and every row opens
the document page that already owns it. The question it answers is "which document happened", never
"which money moved": movements of cash (`transactions`) are deliberately out of scope and stay in the
`finance` capability.

## Ownership boundary
The index owns no table, no column and no write path. Each family's SQL stays in the repository that
owns its table — `sale_repo.rs` (sales and their payments), `purchase_repo.rs` (purchases and their
payments), `customer_receipt_repo.rs` (receipts) and `stock_repo.rs` (movements) — and
`services/documents.rs` only composes those reads. The receipt total is an exception the boundary
still honours: it comes from `SaleRepository::receipt_allocations`, so the receipts file keeps its
rule of never querying a sales table. The index is the one place that joins a counterpart name
(customer, supplier, product) into its own projection: it is a read-only join that keeps the page at
one query per family instead of one per row, and it moves no write ownership.

The index reaches those families through the repositories that own them, not through their
services: a service-level document read expands one query per row — the sales and purchases list
readers return a full detail per document — which is exactly the cost this page's read bound exists
to avoid. The precedent is the receipts repository, which already delegates its `sale_payments` read
to the sales repository so its own file never queries a sales table. That is the index's one
departure from the "exclusively through their services" reading of the architecture invariants, and
it is recorded here rather than left implicit: every statement still lives in the repository that
owns its table, the index writes nothing, and a page render never expands one query per document.

No file of this capability reads an identity table: the acting user's display name is resolved by
the route layer (`routes::audit_actor_names`, `routes::audit_actor_ids`), the only layer allowed to.

## The families and the codes that open them
| Family (`DocumentKind`) | Table | Code that reads it |
| --- | --- | --- |
| `Sale` | `sales` | `sales.read` |
| `SalePayment` | `sale_payments` | `sales.read` |
| `Purchase` | `purchases` | `purchases.read` |
| `PurchasePayment` | `purchase_payments` | `purchases.read` |
| `StockMovement` | `stock_movements` | `inventory.read` |
| `Receipt` | `customer_receipts` | `customers.read` |

No new permission was added for the index, and the catalog stays at its 23 codes: the screen is an
index OF documents that other tiers already own, so it inherits their read codes instead of inventing
a gate that would have to re-derive who may see what.

## The filter vocabulary
The operator thinks in four options, and they PARTITION the six families: `DocumentGroup::Sales` is
the sale document, `Purchases` the purchase document, `Stock` the movements, and `Payments` the three
payment families together (sale payments, purchase payments and collection receipts). A partition,
not an overlap: a row is listed under exactly one option, so expanding two selected options can never
list the same row twice, and no family maps to more than one option, so there is no ambiguity to
resolve. The index accepts the six families and reads each one once, in declaration order, whatever
the option list did.

## The feed
- **Order**: date descending, then id descending, then family — a total order, so the merge is
  deterministic and two documents saved the same day never swap between renders.
- **Cap**: the newest `DOCUMENTS_PAGE_LIMIT` (200) documents per request. Each family is read with
  `LIMIT cap + 1`, one row more than the page can show, and that extra row is how the service learns
  the history did not end. A feed that exceeded the cap reports `truncated`, and the page says so in
  the operator's words instead of presenting a cut history as a complete one.
- **Reads are bounded**: at most one query per participating family plus one batched children read
  where the money is derived — two for sales, purchases and receipts, one for stock, independent of
  how many documents match. No read path expands a document per row, and no page render touches a
  write surface.
- **Money is summed in Rust**: amounts are TEXT columns, so every total (a sale's lines, a purchase's
  lines, a receipt's allocations) is folded in Rust over `Decimal`, reusing the one definition the
  owning flow already uses (`SaleLine::subtotal`, `PurchaseLine::subtotal`). SQL `SUM()` over an
  amount column is forbidden here exactly as it is everywhere else.
- **Failure is whole**: a family that fails makes the request fail. A short list is never the silent
  consequence of a repository error.

## Rows and drill-down
Every row carries the family, its identifier (the document number, or `Draft #id` while a draft has
none; `Recibo #id` for a receipt), the counterpart (customer, supplier or product), the date, a
status/detail line (the document status; `Pago`; `Cobro`; `In · Purchase` for a movement), the money
amount or — for stock, the only family whose magnitude is not money — the quantity, and the acting
user's display name. The identifier is a button that opens the drawer (below); `Open` links to the
page that owns the document: `/sales/{id}`, `/purchases/{id}`, `/customers/{id}`. A stock movement
links to the products list at that product's row, because no product record page exists: `/products`
is the list and the product detail is an HTMX drawer fragment. Every link is gated by the same code
that made the row visible, so no row points at a page its reader would be refused.

## The drawer
Clicking a row's identifier opens the side drawer the sibling list pages use: a fixed right panel
(`#document-drawer`) whose body the row's `hx-get` swaps into (`#document-drawer-body`). Escape
closes it and empties the body; there is no backdrop; `base.html` is untouched. The fragment is
`GET /web/documents/detail/{kind}/{id}`, where `{kind}` is the family's URL token — parsed by
`DocumentKind::parse`, so an unknown token is a 404 naming the token, and a known family with no
such document is a 404 naming it.

The drawer renders through ONE partial (`partials/document_detail.html`) for all six families: the
route assembles a generic payload — a title (the identifier), a status line, a list of
`(label, value)` facts, optional tables, an optional parent summary, an optional notice sentence
and a list of links — so six near-identical per-family templates cannot drift. The operator-facing
copy (fact labels like "Registrado por", "Total", "Pagado", "Saldo"; section titles like
"Líneas", "Pagos", "Asignaciones") is Spanish like the record pages the drawer mirrors, while the
page chrome stays English like the sibling list pages. Every display name is resolved in the route
layer before the template runs: actor names only through `routes::audit_actor_names` (AC20 holds —
no file of this capability reads an identity table), product/account/method names through the same
service reads the record pages use, the parent payment view matched by id, and the ledger
transactions through `TransactionService::get`. Money is summed in Rust by the owning services;
the drawer invents no new total.

What each family shows:
- **Sale** — number (or `Draft #id`), status, customer, payment type, dates, receipt no., notes,
  cancel reason, total/paid/due/payment status, actor names, and the lines (product, SKU, qty,
  unit price, subtotal) and payments (date, account, method, amount) tables; link to `/sales/{id}`.
- **Purchase** — the mirror with supplier and supplier invoice, and unit costs; link to
  `/purchases/{id}`.
- **SalePayment** — the payment's own amount, date, account and method (matched from the parent
  record's resolved payment views), the finance `Income` it produced and the refund `Expense` when
  the sale was cancelled (each shown as kind · amount · date and linked to `/accounts/{account_id}`
  — the drawer links to the ledger, it never invents an account-name read), the receipt that
  grouped it when one did (linked to `/customers/{customer_id}`), plus the parent sale's summary as
  a sub-block (number, status, total, paid, due) with its own link.
- **PurchasePayment** — the mirror with purchase-family links.
- **StockMovement** — product (name + SKU), type, reason, quantity, reference, date, actor, and the
  product's current derived stock; a plain sentence states that the movement is append-only history
  (no edit, no delete), the guarantee the drawer's action slice builds on; link to
  `/products#product-{product_id}`.
- **Receipt** — customer, date, account, method, notes, total (derived from its allocations), actor,
  and the allocations table where each row names its sale and links to `/sales/{sale_id}`; link to
  `/customers/{customer_id}`.

Per-family narrowing: the drawer obeys the same rule the rows obey. A family whose `read_code` the
principal lacks is refused with the standard 403 in the same voice as the extractor's ("Se necesita
el permiso «sales.read» para ver este documento"), and the mapping it quotes is
`DocumentKind::read_code` — the single family→code mapping the list's `permitted_kinds`, the drawer
route and the kernel agreement test share. Nothing a request supplies can widen it: the kind comes
from the URL, and the URL's token decides nothing the principal's codes have not already decided.

The index itself still owns no table: the drawer's reads ride the repositories and services that own
each family (including the only two reads it added — one payment by id in `sale_repo.rs` and
`purchase_repo.rs`, both one query), and its one WRITE delegates entirely — the draft delete goes
through `SaleRepository::delete_draft` / `PurchaseRepository::delete_draft`, whose `WHERE status =
'Draft'` is the load-bearing backstop that makes deleting a confirmed document impossible even if a
caller relaxed the service's state guard.

## The drawer's actions
The action block offers ONLY the actions that really exist, each built only when the principal holds
the code its endpoint requires and the document's state allows it — a rendered button is always a
button that works, and the drawer never invents one. Three rules, all deliberate:

1. **Delete is real only for a DRAFT** (`DELETE /web/sales/{id}` → `sales.create`,
   `DELETE /web/purchases/{id}` → `purchases.create`): a draft has no payments (`record_payment`
   refuses anything not `Confirmed`), no stock movement and no ledger entry (both are created by
   `confirm`), and nothing else references it but its CASCADE children — so a draft delete leaves
   nothing dangling. The service refuses any other state with a `Validation` naming it, and the
   SQL backstop above holds even without the service check.
2. **A confirmed document is ANULLED, never deleted** (`POST /web/sales/cancel` → `sales.cancel`,
   `POST /web/purchases/cancel` → `purchases.cancel`, with the optional reason stored in
   `cancel_reason`): `cancel` is the designed inverse — one `In · Sale-return` / `Out
   · Purchase-return` movement per tracked line, one `Expense`/`Income` refund per payment,
   `cancel_reason` and the actor recorded. For a DRAFT the same endpoint is offered as
   "Descartar": a state change with no stock, money or ledger to revert.
3. **Payments, receipts and stock movements get NO delete** — the drawer says why instead of
   rendering a dead button: a payment's money is already in the ledger and the entry stays (the
   refund of an annulment is recorded by the annulment, not by hand), a receipt groups payments
   the database refuses to orphan while it explains them, and stock history is append-only — the
   drawer points at the product's adjustment path to compensate instead.

**The impact preview**: every action renders its impact ABOVE the button, computed on the server
from the same reads the endpoint will use — for the delete, the draft and its N listed lines with
the sentence of what a draft never did; for the annulment, one line per tracked line (the line
view's `tracks_stock` is exactly the confirm/cancel predicate), one line per payment naming amount
and account, the state change that stops the debt, and a `No se puede anular` blocker line per
tracked product that is NOW inactive (resolved through `inventory_service.get_product` — the same
precondition `cancel` refuses on). A blocker never hides the action: the operator sees the refusal
before pressing. With `allow_negative = false`, the sale annulment also previews the
negative-balance refusal (purchases refund Income, so no such caveat exists to state).

The edit affordance is a state-labelled BUTTON-STYLED LINK to the record page ("Editar cabecera"
for a draft, "Abrir el documento" otherwise) plus one sentence saying the multi-field actions live
there: the drawer duplicates no edit form. After any action the endpoints answer with
`HX-Trigger` (`sale-changed` / `purchase-changed`); the page closes the drawer and re-reads the
feed.

## Filters
- **Type** (`group`): one of the four options, or absent for all of them. The requested option is
  intersected with the permitted families; a request for a family the principal may not read renders
  an empty list, never a refusal of the whole page.
- **Acting user** (`user`): a normalized substring of the user's display name or username. It is
  resolved to actor ids in the route layer and the document query narrows on `created_by`. A name
  that matches no user is a filter matching nothing — an empty list, never an error and never a
  silent "show everything".
- **Date range** (`from`, `to`): inclusive bounds on each family's own date column. An empty or
  unparseable value is treated as absent, so a partial URL is never an error.
- **Search** (`q`): case-insensitive, escaped, matched against the document's identifier/reference
  and its counterpart's name — sales by number, receipt number and the frozen customer name;
  purchases by number, supplier invoice and the supplier's name; payments by the parent document's
  number (and invoice) and its counterpart; receipts by the customer's name and the notes; stock
  movements by the product's name, its SKU and the movement's reference.
- The four filters compose, and the result is addressable: `/documents?group=…&user=…&from=…&to=…&q=…`
  reloads the same list, and Back restores the list a filter produced.

## Authorization
The page and its fragment declare the any-of gate `RequireAny<(SalesRead, PurchasesRead, InventoryRead,
CustomersRead)>`: ANY ONE of the four read tiers opens the screen, and a principal holding NONE of them
is refused both routes (the page and the fragment the filter form fetches). The declared codes are the
same ones the sidebar entry declares — the nav row is an `Any` row, the first in `NAV_ENTRIES`, and
the ac21 invariant proves for each of the four codes that holding it ALONE opens the page and renders
that tier's own part of the screen (its family option present, another tier's absent), and that the
empty set opens neither route. Content narrowing is a server decision from the principal's codes
(`permitted_kinds`): nothing a request supplies — a query parameter, a form field, a header — can
widen the feed. The route → permission table in `identity/spec.md` is the authority.

## Non-goals
- A principal holding one tier never sees another tier's family, not even as an option: the screen
  shows the shape of what the reader may see, so it never advertises data it would refuse.
- No write action is INVENTED here, and the index itself still owns no table and writes nothing of
  its own: the drawer offers each document's own existing actions, each gated by the code its
  endpoint requires — the draft delete is the only document delete in the system (its `WHERE
  status = 'Draft'` backstop makes a confirmed delete impossible at the SQL level), a confirmed
  document is annulled instead, and payments, receipts and stock movements are never deletable from
  here. Confirming, editing the header, adding lines, collecting or adjusting stays on the page
  that owns the document.
- No cash ledger. `transactions` is out of scope by decision; the index answers which document, the
  ledger answers which money.
- No pagination: the feed is the newest 200 with an explicit notice when the cap cut. The project has
  no pagination primitive, and inventing one here would make this page the only list that scrolls.

## Verification
`src/services/documents.rs` (merge order, family narrowing, dedup, the empty filter reading nothing,
the cap and its `truncated` flag, filter pass-through) and one projection test per family in the
repository that owns it, including the bounded read counts observed through the repositories'
test-only counters. The route layer is covered in `src/routes/documents_web.rs` and by the smoke
suite, which drives the page and the fragment over HTTP for a single-tier principal, for a principal
holding none of the codes, for each filter, and for a capped feed. The kernel's ac21 invariant in
`src/security/authz.rs` ties the nav row to the route and proves the any-of promise per code. The
page is registered in the smoke suite's wiring guard, so an `hx-*` target no route serves fails the
build.
