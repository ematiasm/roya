# Proposal: redesign-interface

## Workflow
ODD with OpenSpec artifacts. Slices sized for review, independent verification per slice, archive on
merge. Tailwind CSS must be regenerated in every slice that changes a template class.

## Problem statement
The interface leaks the data model's keys into the human's workflow. Loading a sale means knowing and
retyping a database id four times: create the draft, type its id to add a line, type it again to confirm,
again to pay, again to cancel. Purchases work the same way. The concrete findings, read from the current
templates:

1. **Typed database ids.** The sales page holds four always-visible forms — add line, confirm, record
   payment, cancel — and each one asks for `Sale ID` in a number input. Purchases are identical. There is
   no relationship between the record you are working on and the form you are filling.
2. **Ids on screen.** The sale detail prints `product #3` and `account #2 • method #1` instead of the
   product name, the account name and the method name. The user must memorise that 3 is the yerba.
3. **No page for a record.** The "detail" is a fragment in a side panel of the list page. There is no
   `/sales/{id}`, so no back button, no bookmark, no shareable link.
4. **The product picker is a `<select>` of every product.** Unusable with a real catalogue, and the
   `product_barcodes` table the system already stores is never used by the interface.
5. **Errors are `alert()`.** A blocking modal that does not say which of the four forms failed.
6. **Navigation is eight loose pills** mixing modules with environment badges, with no hierarchy.
7. **No search anywhere.** A sale cannot be found by number, customer or date; a product not by name,
   SKU or barcode.

The friction is worst in the three flows the user performs all day: loading a sale, loading a purchase,
and creating a customer.

## Goal
An interface that never asks for an internal key, always shows names, gives each record a real page, and
makes the line-entry loop usable with a keyboard and a USB barcode reader.

## Principles applied
1. **The interface never shows or asks for an internal key.** No typed ids, no ids on screen.
2. **One primary action per screen.** The next obvious thing is always in the same place.
3. **Entry density.** Compact rows on transactional tables, comfortable rows on read-only lists.
4. **Destructive actions ask first.** Cancelling a sale is currently a bare button.
5. **Prefer server-rendered fragments to custom JavaScript.** HTMX plus Askama handle the entry loop;
   vanilla JS is added only where HTML cannot do the job, and it stays small enough to read in one sitting.

## Scope

### N1 — Shell
Grouped sidebar: **Operation** (Dashboard, Sales, Purchases), **Catalogue** (Products, Suppliers,
Customers), **Cash**. Fixed on desktop, collapsed to icons on tablet, off-canvas on mobile. Page header
with title, primary action and a breadcrumb inside a record. Errors become a dismissible notice naming
the failed action instead of a blocking `alert()`. The stale `Personal Finance` document title is fixed.
Responsive for a PC first, usable on a tablet and a phone.

### N2 — Record pages
Real `/sales/{id}` and `/purchases/{id}` pages: header with status, number, party, dates and totals; the
lines as a table; and the actions (confirm, pay, cancel) in context, gated by status. Creating a sale or a
purchase redirects to its page, so an id is never typed. Quick-create for a customer from the sale page,
so a missing customer does not force a detour.

### N3 — Line entry
One product field that matches on name, SKU and barcode at once. The USB reader types like a keyboard, so
scanning fills the field and adds the line; focus returns to the field for the next one. Enter adds, Escape
clears. The line table shows subtotals and a running total while you load.

### N4 — Names and search
Every fragment shows names instead of ids. Filters on the lists: sales by status, customer, number and
date; purchases the same; products by name, SKU and barcode.

## Out of scope, parked deliberately
- **Counter mode (POS).** A single fast screen for the most frequent operation. The user wants it later;
  it is a new page with its own flow and deserves its own change.
- **Ticket printing.** Wanted, but optional and later.
- **PWA.** Installability needs a service worker and a secure origin, and the practical constraint is that
  a tablet reaching the app over the LAN is not a secure origin. Parked until counter mode exists, and
  even then it buys an installable shell, not offline data entry, because stock, balance and debt are
  derived on the server.

## Side effects worth anticipating
The smoke suite's wiring guard has a special rule for pages where the id is typed. Once those pages are
gone the rule simplifies and a class of risk disappears. In exchange, every new page must be added to the
guard's seeded page list.

## Impact
All 22 templates, several web route handlers, two new record pages, one new search endpoint, the smoke
suite and its guard rules, and a Tailwind rebuild per slice. **No migrations and no schema change.**
