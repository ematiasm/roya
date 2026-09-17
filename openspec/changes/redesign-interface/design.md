# Design: redesign-interface

## Shell
```
+----------------+--------------------------------------------+
| Operation      |  Page header: title · primary action       |
|  Dashboard     |  breadcrumb when inside a record           |
|  Sales         +--------------------------------------------+
|  Purchases     |                                            |
| Catalogue      |  page content                              |
|  Products      |                                            |
|  Suppliers     |                                            |
|  Customers     |                                            |
| Cash           |                                            |
|  Accounts      |                                            |
|                |                                            |
| local · SQLite |                                            |
| REST API       |                                            |
+----------------+--------------------------------------------+
```
- One template, `partials/sidebar.html`, included by `base.html`. The active entry is decided on the
  server: each page's template struct carries the active key, so the state is correct without JavaScript
  and a test can assert it. Askama cannot read the request path, so this is passed explicitly rather than
  guessed.
- Responsive with Tailwind classes. Desktop `lg:` and up shows the full sidebar; `md:` shows icons only;
  below `md:` it is a drawer toggled by a button in the header. One small script toggles the drawer and
  closes it on navigation; the CSS carries the layout.
- The page header is a component: title, optional breadcrumb, and one primary action. Having a single slot
  for the primary action is what keeps "the next obvious thing" in the same place on every page.

## Record pages
- New handlers render `/sales/{id}` and `/purchases/{id}` as full pages reusing the existing detail data
  the services already expose. The list page keeps the list and loses the side-panel detail, replacing it
  with a link per row.
- Creating a document answers `HX-Redirect: /sales/{id}` so htmx performs a real navigation to the new
  record. Full navigation is chosen deliberately over swapping a fragment: it gives the back button, a
  reloadable page and a shareable URL, which is the whole point of the slice.
- Actions are gated by status in the template **and** re-checked by the service, which already refuses an
  edit of a confirmed document. The template gate is presentation; the service remains the authority.
- Destructive actions use `hx-confirm`, so the confirmation is declarative and needs no custom script.

## Line entry
The loop is server-driven, which keeps it testable and avoids a JavaScript cart:
- The product field issues `hx-get` against a new search endpoint with a debounce, and swaps a results
  fragment under the field.
- Submitting the field is still a normal form post: the server resolves the value as a barcode first, then
  as a SKU, then as an id, and adds the line. That is what makes an exact barcode a single step.
- The response returns the updated line table **and** an out-of-band swap of the picker with an empty,
  focused field, so the cursor is ready for the next item without a script.
- The running total comes from the same response, so it cannot drift from the lines.
- Escape clears the field with a tiny script; everything else is HTML.

Search endpoint: `GET /web/product-search?q=`, read-only, returning at most a bounded number of matches
with name, SKU, price and current stock, so the picker is useful without inventing a full search engine.
Matching reuses the existing repository reads; a barcode lookup already exists in the inventory module.

## Feedback
- `base.html` gains a `#notice` region. The global `htmx:responseError` handler stops calling `alert()` and
  swaps the error into that region, prefixed with the action that failed.
- Success uses `HX-Trigger` events that already exist for refreshing regions, plus an optional notice.
- The notice is dismissible and does not block interaction. Field-level errors stay where the design
  already validates: the service returns 400 with a message and the form keeps its values.

## Tradeoffs
| Option | Chosen | Why / cost |
|---|---|---|
| Full page per record vs fragment in a side panel | Full page | Back button, reload, bookmark; cost: a full render per navigation instead of a partial swap |
| Server-driven cart vs a JavaScript cart | Server-driven | No duplicated total logic, testable through HTTP; cost: a round trip per line, acceptable on a local server |
| Declarative `hx-confirm` vs a custom modal | `hx-confirm` | Zero script; cost: a native browser dialog, plainer but honest |
| Active nav decided server-side vs by script | Server-side | Correct without JS and assertable; cost: one field per template struct |
| Keep the right-rail panel on reference pages | Keep | It fits catalogue editing; only transactional pages change model; cost: two layout idioms coexist |
| Counter mode now | Parked | It is the highest-value later addition but a new flow; mixing it in would double the review surface |
| PWA | Parked | Buys an installable shell, not offline entry; a tablet on the LAN is not a secure origin anyway |

## Impact on existing code
- `base.html` and all 22 templates change for the shell; the transactional pages change more deeply.
- Web route handlers gain the active nav key and the two record-page handlers; one search handler is added.
- The smoke suite's wiring guard loses its typed-id rule and gains the new pages in its seeded list.
- The compiled stylesheet is regenerated and committed in each slice.
- No migration, no schema change, and no service rule changes: this is presentation plus routing.
