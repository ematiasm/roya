# Spec: redesign-interface

## Requirements

### R1 — Shell and navigation
- Every page renders inside one shell: a grouped sidebar plus a content area with a page header.
- The sidebar groups entries as **Operation** (Dashboard, Sales, Purchases), **Catalogue** (Products,
  Suppliers, Customers) and **Cash** (accounts). Environment badges and the API link move to the shell
  footer, out of the navigation group.
- The entry matching the current page is marked as active **on the server**, so the state is correct
  without JavaScript and can be asserted in a test.
- Layout: fixed sidebar at 1024 px and above; collapsed to icons between 768 and 1023 px; off-canvas
  behind a toggle below 768 px. No horizontal scrolling of the page body at 360 px width.
- The document title block no longer says `Personal Finance`.

### R2 — No typed identifiers
- No form in the interface requires the user to type a database id.
- Every rendered page shows names for products, accounts, payment methods, customers and suppliers. A
  bare `#id` for any of those entities is a defect.
- Creating a sale or a purchase navigates to that record's page rather than returning the user to a list.
- Inside a record, only the actions valid for its status are offered, and a destructive action asks for
  confirmation before it runs.

### R3 — Sale and purchase record pages
- `/sales/{id}` and `/purchases/{id}` are real pages reachable by URL, with the browser back button
  working and the page reloadable without losing state.
- Each shows the document status, its number or its draft state, the party, the dates, the money totals
  and the payment status.
- Lines are presented as a table with product name, quantity, unit price and subtotal.
- The available actions depend on status: a draft can be edited, confirmed or discarded; a confirmed
  document can be paid or cancelled; a cancelled one is read-only.
- An unknown id answers 404 with the existing error shape.

### R4 — Line entry
- A single product field matches by name, SKU and barcode, and shows the matches as the user types.
- Submitting an exact barcode adds that product directly, without requiring a selection step.
- After a line is added the product field is empty and focused again, so a whole sale can be loaded
  without touching the mouse.
- Enter submits the line; Escape clears the field.
- The line table and the running total update on every added or removed line.
- A product that does not exist returns a clear message and does not add a line.

### R5 — Names and filters
- Detail fragments and record pages resolve product, account, method, customer and supplier names.
- Sales and purchases can be filtered by status, party, document number and date.
- Products can be filtered by name, SKU and barcode, retaining the existing category filter.

### R6 — Feedback
- No `alert()`. A failed action renders a dismissible notice that names the action and carries the error
  message.
- A successful action renders a short confirmation notice, or updates the affected region visibly.
- Validation errors do not lose what the user already typed.

### R7 — Verification and delivery
- Every new page and fragment is added to the smoke suite's wiring guard, and the guard passes.
- The typed-id exception in the guard is removed, because no page requires a typed id any more.
- The compiled stylesheet is regenerated in each slice and committed, so `cargo run` keeps working
  without the Tailwind CLI installed.
- No CDN reference is introduced; the application keeps working with no network.

## Acceptance criteria
- [ ] AC1: every page renders inside the shell, and each page's sidebar entry is marked active server-side.
- [ ] AC2: at 1024 px the sidebar is full, at 768 px it is collapsed, below 768 px it is off-canvas, and no
      page scrolls horizontally at 360 px.
- [ ] AC3: grep over the rendered pages finds no form input named for a typed id, and no `#`-prefixed id
      for a product, account, method, customer or supplier.
- [ ] AC4: creating a sale lands on `/sales/{id}`; creating a purchase lands on `/purchases/{id}`.
- [ ] AC5: `/sales/{id}` and `/purchases/{id}` return 200 for an existing record and 404 for an unknown one.
- [ ] AC6: a draft offers edit and confirm; a confirmed document offers pay and cancel and does not offer
      line editing; a cancelled one offers neither.
- [ ] AC7: cancelling asks for confirmation before the request is sent.
- [ ] AC8: typing a product name, a SKU or a barcode all produce the matching product in the picker.
- [ ] AC9: submitting an exact barcode adds the line in one step.
- [ ] AC10: after adding a line the picker is empty and focused, and the running total reflects the new line.
- [ ] AC11: removing a line updates the total.
- [ ] AC12: an unknown product returns a clear message and adds nothing.
- [ ] AC13: list filters return the expected subsets for status, party, number and date.
- [ ] AC14: product filters match on name, on SKU and on barcode.
- [ ] AC15: no template or script calls `alert()`, and a failed request renders a notice naming the action.
- [ ] AC16: the wiring guard covers every new page and passes, with the typed-id rule removed.
- [ ] AC17: `cargo test` is green at every slice, and the compiled CSS is up to date with the templates.
