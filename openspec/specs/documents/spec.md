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
user's display name. `Open` links to the page that owns the document: `/sales/{id}`, `/purchases/{id}`,
`/customers/{id}`. A stock movement links to the products list at that product's row, because no
product record page exists: `/products` is the list and the product detail is an HTMX drawer
fragment. Every link is gated by the same code that made the row visible, so no row points at a page
its reader would be refused.

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
- No write action lives here. Confirming, cancelling, collecting or adjusting stays on the page that
  owns the document; the index is a way in, not a second place to do it.
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
