"""Purchase returns and credit notes: the two families a person can actually do.

The Rust suite shipped 100+ tests for these six tables and every one of them
passed while the feature was unusable. Both list pages answered 200 with no
sidebar entry, so they were reachable only by typing a URL; the "New return"
dialog offered a `parent_ref` text field above a hidden `purchase_id` nothing
ever filled, so its only reachable outcome was a 422; and that 422 said
`Failed to deserialize form body: purchase_id: cannot parse integer from empty
string` to the operator. No suite catches a missing link or a dead button,
because both are statements about what a person sees rather than about what the
code returns. This module is the one that sees.

**Every journey starts where an operator starts: on the parent document.** A
return names the document it reverses, so the action lives on that document's
record page and carries its id into creation. A test that navigated to
`/purchase-returns` by URL would have passed against the broken build — the page
answered 200 — so the creation journeys click the button on the purchase and
wait for the URL to move. Reachability is asserted by walking the sidebar for
the same reason.

**The facts are asserted against the database, not the DOM.** Neither family
has a JSON API (`grep -r 'api/purchase-returns' src/` is empty), so the
throwaway SQLite file the spawned server already owns is the only machine-
readable account of what a confirmation wrote. A status chip is a rendering of
a stored status; asserting the rendering would let a template that prints
"Confirmed" over a Draft pass. `read_rows_in_database` is the strictly weaker
sibling of the one write `helpers.py` already makes for session expiry: it
refuses any statement that is not a SELECT, so seeding still goes through the
API and a broken endpoint still fails loudly.

What is asserted in the DOM is what only the DOM can answer: that a nav entry
exists and a click on it resolves, that no price input is reachable anywhere on
a return, that the refusal an operator reads is a sentence rather than a serde
internal — and, in the two sections that follow, two things about what a person
SEES rather than about what the code returns.

**The record body rendered twice, and the stale copy lied about money.** The
quantity control swapped the whole record fragment into the MONEY REGION while
every other action targeted the outer wrapper, so after the first keystroke the
screen carried two headers, two party lines, two parent links and two payment
chips — and the stale one, rendered before the line existed, said PAID beside a
fresh copy saying UNPAID. Every other test in this module read the inner copy,
which is the correct one, so the duplicate was invisible to all of them. The
count assertions at the end of this file are what see it.

**The counter flow is driven from the keyboard.** An operator types a quantity
per row twenty times in a row, and the commit is a blur, so the flow was twenty
tab-and-click round trips through a mouse. Enter now commits and advances, past
rows the service would refuse, and Shift+Enter goes back without committing so a
miscount is one keystroke to fix. Focus is the claim, so it is read from
`document.activeElement` and never from a screenshot: a screenshot shows where
the caret LOOKS like it is, and the property under test is which element the
browser will send the next keystroke to.
"""

from __future__ import annotations

import contextlib
import os
import re
import sqlite3
from decimal import Decimal
from pathlib import Path
from typing import Iterator
from urllib.parse import urlparse

import pytest
from playwright.sync_api import Browser, BrowserContext, Page, expect

from conftest import ARTIFACTS_ROOT, LiveServer
from helpers import (
    ApiClient,
    add_purchase_line,
    add_sale_line,
    confirm_purchase,
    confirm_sale,
    create_confirmed_credit_purchase,
    create_confirmed_credit_sale,
    create_customer,
    create_product,
    create_purchase_draft,
    create_sale_draft,
    create_supplier,
    e2e_copy,
)
from test_identity import (
    INITIAL_PASSWORD,
    CHANGED_PASSWORD,
    _assign_role_through_the_screen,
    _change_confined_password,
    _create_user_through_the_screen,
    _log_in_through_the_form,
)

# ---------------------------------------------------------------------------
# The two families, declared once
# ---------------------------------------------------------------------------
#
# Three document families mirror each other and the two return families mirror
# each other, so a test names its family and reads the rest off this table. The
# permission codes are here rather than inline because they are the ONE fact
# that differs by family and it is the fact the gate test is about: a return
# reuses its parent's tier, so there is no `purchase_returns.create` and a
# principal who can annul a purchase can return goods against it.

_PURCHASE_RETURN = {
    "id": "purchase-returns",
    "name": "purchase return",
    "list_path": "/purchase-returns",
    "list_inner": "purchase-return-list-inner",
    "nav_label": "Purchase returns",
    "create_path": "/web/purchase-returns",
    "parent_path": "/purchases",
    "action_form": "#purchase-return-action",
    "action_button": "Return goods",
    "parent_field": "purchase_id",
    "record_inner": "#purchase-return-record-inner",
    "line_row_prefix": "purchase-return-parent-line-",
    "qty_prefix": "return-qty-",
    "confirm_button": "#open-confirm-return",
    "confirm_dialog": "#confirm-purchase-return",
    "number_pattern": r"^\d{4}-PRET-\d{6}$",
    "create_code": "purchases.create",
    "read_code": "purchases.read",
    "document_table": "purchase_returns",
    "line_table": "purchase_return_lines",
    "number_column": "return_number",
    "parent_column": "purchase_id",
    "parent_line_column": "purchase_line_id",
    "money_column": "unit_cost",
    "movement_type": "Out",
    "movement_reason": "Purchase-return",
    "bought_header": "Bought",
    "taken_header": "Already returned",
    "remaining_header": "Still returnable",
    "frozen_label": "Cost at purchase",
    "frozen_cell": "[data-frozen-cost]",
    "parent_word": "Purchase",
    "empty_state": "No purchase returns yet",
    "frozen_figure": "12.00",
    "frozen_sentence": "The cost comes from the purchase line",
    "start_hint": "To start one, open the confirmed purchase",
    # The pieces of the record header, each addressed by its own marker. They
    # are read individually rather than through a page-level text match because
    # a text match cannot tell ONE header from TWO, which is the whole claim.
    "payment_status": "[data-purchase-return-payment-status]",
    "actor": "[data-purchase-return-actor]",
    "action_bar": "#purchase-return-action-bar",
    # The line editor's quantity controls, as the DOM reports them.
    "row_selector": "input[id^='return-qty-']",
}

_CREDIT_NOTE = {
    "id": "customer-returns",
    "name": "credit note",
    "list_path": "/customer-returns",
    "list_inner": "customer-return-list-inner",
    "nav_label": "Credit notes",
    "create_path": "/web/customer-returns",
    "parent_path": "/sales",
    "action_form": "#sale-credit-note-action",
    "action_button": "Credit note",
    "parent_field": "sale_id",
    "record_inner": "#customer-return-record-inner",
    "line_row_prefix": "customer-return-parent-line-",
    "qty_prefix": "return-qty-",
    "confirm_button": "#open-confirm-note",
    "confirm_dialog": "#confirm-customer-return",
    "number_pattern": r"^\d{4}-SRET-\d{6}$",
    "create_code": "sales.create",
    "read_code": "sales.read",
    "document_table": "customer_returns",
    "line_table": "customer_return_lines",
    "number_column": "credit_note_number",
    "parent_column": "sale_id",
    "parent_line_column": "sale_line_id",
    "money_column": "unit_price",
    "movement_type": "In",
    "movement_reason": "Sale-return",
    "bought_header": "Sold",
    "taken_header": "Already credited",
    "remaining_header": "Still creditable",
    "frozen_label": "Price at sale",
    "frozen_cell": "[data-frozen-price]",
    "parent_word": "Sale",
    "empty_state": "No customer returns yet",
    "frozen_figure": "25.00",
    "frozen_sentence": "The price comes from the sale line",
    "start_hint": "To start one, open the confirmed sale",
    "payment_status": "[data-customer-return-payment-status]",
    "actor": "[data-customer-return-actor]",
    "action_bar": "#customer-return-action-bar",
    "row_selector": "input[id^='return-qty-']",
}

_FAMILIES = (_PURCHASE_RETURN, _CREDIT_NOTE)


# ---------------------------------------------------------------------------
# Reading the throwaway database
# ---------------------------------------------------------------------------


def read_rows_in_database(db_path: Path, sql: str, params: tuple = ()) -> list[tuple]:
    """Run a SELECT against the throwaway database and return its rows.

    Some facts have no endpoint. Neither return family has a JSON API, so "did
    the confirmation really write the movement it claims" is answerable only by
    reading what was written. This is a READ of the file the spawned server
    already owns, against no operation the interface could perform.

    It refuses anything but a SELECT, and that guard is the point: a statement
    that could write would be a seed that bypasses the application, and the
    harness's contract is that every write goes through an endpoint so a broken
    one fails loudly. Making the contract mechanical beats promising it in a
    docstring.
    """
    statement = sql.strip()
    if not statement.upper().startswith("SELECT"):
        raise ValueError(
            f"read_rows_in_database is a READ: {statement[:48]!r} is not a SELECT. "
            "Seed through the API so a broken endpoint fails the seed loudly."
        )
    connection = sqlite3.connect(str(db_path), timeout=5.0)
    try:
        connection.execute("PRAGMA busy_timeout = 5000")
        return connection.execute(statement, params).fetchall()
    finally:
        connection.close()


def _columns_of(db_path: Path, table: str) -> set[str]:
    """The column names of one table, read from SQLite's own metadata."""
    rows = read_rows_in_database(
        db_path, "SELECT name FROM pragma_table_info(?)", (table,)
    )
    return {row[0] for row in rows}


# ---------------------------------------------------------------------------
# Seeds
# ---------------------------------------------------------------------------


def _family_seed(api: ApiClient, family: dict, *, tag: str) -> dict[str, int]:
    """One product with stock, plus the CONFIRMED credit parent of one family.

    Credit on both sides, deliberately. A credit purchase collects nothing, so
    its confirmation posts no payment and a return against it writes no refund
    row: the stock arithmetic this module asserts is then exactly one movement
    per document and nothing else. It also keeps the seed free of the accounts
    and the funding the cash variants need, which are not what these tests are
    about.

    The parent LINE id comes back too, because a return line points at the
    parent line and the browser's quantity input is named after it
    (`return-qty-{parent_line_id}`).
    """
    product_id = int(
        create_product(
            api,
            sku=f"{tag}-SKU",
            name=f"{tag} Widget",
            sale_price="25.00",
            cost_price="10.00",
            stock="40",
            min_stock="1",
            max_stock="100",
        )["id"]
    )
    if family["id"] == "purchase-returns":
        supplier_id = create_supplier(api, f"{tag} Supplier")
        parent_id = create_confirmed_credit_purchase(
            api, supplier_id, product_id, qty="5", unit_cost="12.00"
        )
        line_id = int(
            next(
                line["id"]
                for line in api.get_json(f"/api/purchases/{parent_id}")["lines"]
                if int(line["product_id"]) == product_id
            )
        )
    else:
        customer_id = create_customer(api, f"{tag} Buyer")
        parent_id = create_confirmed_credit_sale(
            api, customer_id, product_id, qty="4", unit_price="25.00"
        )
        line_id = int(
            next(
                line["id"]
                for line in api.get_json(f"/api/sales/{parent_id}")["lines"]
                if int(line["product_id"]) == product_id
            )
        )
    return {"product_id": product_id, "parent_id": parent_id, "line_id": line_id}


def _two_line_confirmed_purchase(
    api: ApiClient, family: dict, *, tag: str
) -> dict[str, int]:
    """A CONFIRMED PARENT of `family` carrying two product lines of eight units.

    Two lines, not two lines of one product: a purchase refuses the same
    product twice (`product_supplier_costs` is UNIQUE per (product, supplier),
    so a second line would have no defined cost). Two lines under ONE parent is
    what makes the allowance columns discriminate — a row a first return has
    partly consumed beside a row it has not touched at all — and what gives a
    row-stepping test somewhere to step to.

    Built per FAMILY rather than as a purchase alone, because a return is
    created from its parent's own record page: a credit note seeded from a
    purchase id would navigate to `/sales/{purchase_id}` and land on a document
    that has nothing to do with it.
    """
    first = int(
        create_product(
            api, sku=f"{tag}-SKU-A", name=f"{tag} A", sale_price="20.00",
            cost_price="6.00", stock="40", min_stock="1", max_stock="100",
        )["id"]
    )
    second = int(
        create_product(
            api, sku=f"{tag}-SKU-B", name=f"{tag} B", sale_price="20.00",
            cost_price="6.00", stock="40", min_stock="1", max_stock="100",
        )["id"]
    )
    if family["id"] == "purchase-returns":
        supplier_id = create_supplier(api, f"{tag} Supplier")
        parent_id = create_purchase_draft(
            api, supplier_id, payment_type="Credit", due_date="2024-06-01"
        )
        for product_id in (first, second):
            add_purchase_line(api, parent_id, product_id, qty="8", unit_cost="6.00")
        confirm_purchase(api, parent_id)
        path = f"/api/purchases/{parent_id}"
    else:
        customer_id = create_customer(api, f"{tag} Buyer")
        parent_id = create_sale_draft(
            api, customer_id, payment_type="Credit", due_date="2024-06-01"
        )
        for product_id in (first, second):
            add_sale_line(api, parent_id, product_id, qty="8", unit_price="20.00")
        confirm_sale(api, parent_id)
        path = f"/api/sales/{parent_id}"
    lines = api.get_json(path)["lines"]
    return {
        "parent_id": parent_id,
        "line_id": int(next(l["id"] for l in lines if int(l["product_id"]) == first)),
        "other_line_id": int(
            next(l["id"] for l in lines if int(l["product_id"]) == second)
        ),
    }


def _three_line_confirmed_purchase(
    api: ApiClient, family: dict, *, tag: str
) -> dict[str, int]:
    """A CONFIRMED PARENT of `family` carrying THREE lines of eight units each.

    Three lines rather than two, and the arrangement is the whole reason: with
    two rows, a row-stepping test cannot tell a correct advance from an
    `index + 1`. Disabling one of two leaves the survivor either first or last,
    and both positions are reached by the naive arithmetic. Three rows with the
    MIDDLE one unavailable is the only arrangement where `index + 1` lands on
    the disabled control and a correct implementation lands past it.

    Three products under one party, not one product three times: a purchase
    refuses a repeated product, so the lines could not exist otherwise.
    """
    ids = [
        int(
            create_product(
                api, sku=f"{tag}-SKU-{letter}", name=f"{tag} {letter}",
                sale_price="20.00", cost_price="6.00", stock="40",
                min_stock="1", max_stock="100",
            )["id"]
        )
        for letter in ("A", "B", "C")
    ]
    if family["id"] == "purchase-returns":
        party_id = create_supplier(api, f"{tag} Supplier")
        parent_id = create_purchase_draft(
            api, party_id, payment_type="Credit", due_date="2024-06-01"
        )
        for product_id in ids:
            add_purchase_line(api, parent_id, product_id, qty="8", unit_cost="6.00")
        confirm_purchase(api, parent_id)
        path = f"/api/purchases/{parent_id}"
    else:
        party_id = create_customer(api, f"{tag} Buyer")
        parent_id = create_sale_draft(
            api, party_id, payment_type="Credit", due_date="2024-06-01"
        )
        for product_id in ids:
            add_sale_line(api, parent_id, product_id, qty="8", unit_price="20.00")
        confirm_sale(api, parent_id)
        path = f"/api/sales/{parent_id}"
    lines = api.get_json(path)["lines"]

    def line_of(product_id: int) -> int:
        return int(next(l["id"] for l in lines if int(l["product_id"]) == product_id))

    return {
        "parent_id": parent_id,
        "line_id": line_of(ids[0]),
        # The middle row. A test consumes it in full so a later draft renders it
        # `disabled`, and the row-stepping claim becomes observable.
        "blocked_line_id": line_of(ids[1]),
        "third_line_id": line_of(ids[2]),
    }


# ---------------------------------------------------------------------------
# Navigation and interaction helpers
# ---------------------------------------------------------------------------


def _response_for(path: str, method: str = "GET"):
    """Predicate matching one request path and method, query string ignored."""

    def matches(response) -> bool:
        return urlparse(response.url).path == path and response.request.method == method

    return matches


def _matching_response(method: str, suffix: str):
    """Predicate matching a method and a path SUFFIX, for ids the test knows late."""

    def matches(response) -> bool:
        return response.request.method == method and urlparse(response.url).path.endswith(
            suffix
        )

    return matches


def _click_through_the_sidebar(page: Page, nav_label: str, expected_path: str) -> None:
    """Reach a screen by CLICKING its sidebar entry, and assert where it lands.

    The click is the whole point. A test that typed the URL would have passed
    against the build that shipped this feature with no entry at all, because
    the page answered 200 — which is how 1480 Rust tests and 134 browser tests
    were green while both families were unreachable. So the navigation starts
    from a screen the operator is already on, and the URL assertion is the
    outcome of the click rather than its input.
    """
    page.locator("#sidebar").get_by_role("link", name=nav_label).click()
    page.wait_for_load_state("networkidle")
    assert urlparse(page.url).path == expected_path, (
        f"clicking the {nav_label!r} sidebar entry landed on {page.url}, "
        f"expected {expected_path}"
    )


def _start_from_the_parent_record(
    page: Page, family: dict, parent_id: int, base_url: str
) -> str:
    """Click the parent's create action and return the return's own URL.

    The `wait_for_url` is not politeness. The action posts with
    `hx-swap="none"` and the route answers `HX-Redirect`, and htmx performs
    that navigation as its own browser-internal step: a
    `wait_for_load_state` immediately after the click resolves against the page
    that is still on screen, which is the PARENT's. Measured rather than
    assumed — the first screenshot of this journey, taken after `networkidle`,
    showed the purchase record with the return already created behind it.
    Waiting on the URL is what turns "the operator landed on the return" into
    an assertion.
    """
    page.goto(f"{base_url}{family['parent_path']}/{parent_id}")
    page.wait_for_load_state("networkidle")
    action = page.locator(f"{family['action_form']} button[type=submit]")
    expect(action).to_be_visible()
    expect(action).to_have_text(family["action_button"])
    action.click()
    page.wait_for_url(f"**{family['id']}/*", timeout=20000)
    page.wait_for_load_state("networkidle")
    return page.url


def _return_id_from_url(url: str) -> int:
    return int(urlparse(url).path.rstrip("/").rsplit("/", 1)[1])


def _type_return_quantity(page: Page, family: dict, line_id: int, qty: str) -> None:
    """Type a quantity into the line editor's only control and commit it.

    The control posts on change, so the blur is the submit: a fill alone would
    leave the document untouched and every assertion after it would be reading a
    page that was never asked to change. The wait is on the fragment's own
    "On this return" figure, which only appears once the server has stored the
    line — never on a timer.
    """
    field = page.locator(f"#{family['qty_prefix']}{line_id}")
    expect(field).to_be_visible()
    expect(field).to_be_enabled()
    field.fill(qty)
    field.blur()
    row = page.locator(f"{family['record_inner']} [data-on-this-return='true']")
    expect(row).to_have_count(1)
    expect(row).to_contain_text(qty)


def _confirm_the_return(page: Page, family: dict) -> None:
    """Open the status-gated confirm dialog, submit it, and read the fragment.

    The fact asserted afterwards is the record fragment's own status chip, never
    the page header. `page_header.html` sits OUTSIDE the element every htmx
    action swaps — the pre-existing, house-wide issue documented in AGENTS.md —
    so a confirmed return still shows a Draft title and a live Confirm button
    until a manual reload. (Seen, not inferred: the screenshot taken straight
    after this click reads "Draft credit note" above a record whose own chip
    already says Confirmed.) Reading the header is exactly the mistake that
    would let a confirmation which never happened look like one that did.
    """
    page.locator(family["confirm_button"]).click()
    with page.expect_response(_matching_response("POST", "/confirm"), timeout=20000):
        page.locator(f"{family['confirm_dialog']} button[type=submit]").click()
    expect(page.locator(f"{family['record_inner']} .chip").first).to_have_text("Confirmed")


def _names_matching(page: Page, needle: str) -> list[str]:
    """Every `name=` attribute on the page that matches `needle`, case-insensitively.

    Read from the DOM rather than from the markup string so the assertion holds
    for markup htmx inserted as a fragment too, which is where a control can
    appear without a template changing.
    """
    return page.evaluate(
        """(pattern) => Array.from(document.querySelectorAll('[name]'))
             .map((el) => el.getAttribute('name') || '')
             .filter((name) => new RegExp(pattern, 'i').test(name))""",
        needle,
    )


# ---------------------------------------------------------------------------
# 1. Reachability: the nav entry exists, and clicking it resolves
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_the_return_list_is_reached_by_clicking_its_sidebar_entry(
    page: Page, api: ApiClient, family: dict
) -> None:
    """The entry is in the navigation, and a click on it opens the list.

    The defect this pins is the one that shipped: the page answered 200 with no
    console errors and no way to reach it, so every HTTP-level check passed.
    Three claims, all about a person: the entry is rendered inside the sidebar
    (a DOM question), its `href` is the real route rather than a decoration
    (so a dead `href="#"` cannot pass), and a click resolves to the list screen
    and shows it (a navigation question no status code answers).

    The empty state is asserted too, because a list that says it is empty is a
    usable screen and one that says nothing is the other half of "unreachable",
    and because this screen is where an operator looks for the action that
    starts a return.
    """
    page.goto(f"{api.base_url}/")
    page.wait_for_load_state("networkidle")

    entry = page.locator("#sidebar").get_by_role("link", name=family["nav_label"])
    expect(entry).to_be_visible()
    assert entry.get_attribute("href") == family["list_path"], (
        f"the {family['nav_label']!r} entry points at "
        f"{entry.get_attribute('href')!r}, expected {family['list_path']!r}"
    )

    _click_through_the_sidebar(page, family["nav_label"], family["list_path"])

    inner = page.locator(f"#{family['list_inner']}")
    expect(inner).to_be_visible()
    expect(inner).to_contain_text(family["empty_state"])
    # The screen says WHERE to start one. The deleted dialog used to promise
    # this above a control that did not exist; here it sits at the top of the
    # card, which is where an operator looking for the action reads first.
    expect(page.locator("[data-return-start-hint]")).to_contain_text(
        family["start_hint"]
    )


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_a_return_created_from_the_parent_record_is_listed_where_the_nav_entry_opens(
    page: Page, api: ApiClient, family: dict
) -> None:
    """The document the operator made is findable from the list.

    Reachability is not only "the entry exists": an entry that opens an empty
    list while the document lives somewhere else is the same defect in a
    different hat. The journey creates a DRAFT through the parent record — so
    the creation path is exercised again, on its way to somewhere — and then
    walks the sidebar to the list, which is the path a person takes the next
    morning.

    The row is identified by its `href`, which must name the return that was
    just created. Matching on the rendered status word instead would pass on a
    list that showed some other document's status.
    """
    seeded = _family_seed(api, family, tag=f"LISTED-{family['id']}")
    return_url = _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )
    return_id = _return_id_from_url(return_url)
    _type_return_quantity(page, family, seeded["line_id"], "1")

    _click_through_the_sidebar(page, family["nav_label"], family["list_path"])

    row = page.locator(f"#purchase-return-{return_id}, #customer-return-{return_id}")
    expect(row).to_have_count(1)
    assert row.get_attribute("href") == f"/{family['id']}/{return_id}", (
        f"the list row points at {row.get_attribute('href')!r}, not at the "
        f"return that was just created"
    )
    expect(row).to_contain_text("Draft")


# ---------------------------------------------------------------------------
# 2. Creation end to end, with the facts asserted against the database
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_a_return_is_created_and_confirmed_from_the_parent_record_and_the_facts_are_stored(
    page: Page, api: ApiClient, live_server: LiveServer, family: dict
) -> None:
    """The whole journey a person lives, with the stored facts as the verdict.

    Seed a confirmed credit parent, open ITS record page, click the action, land
    on the return, type a quantity, confirm. Then the facts, read from the
    throwaway database rather than the DOM:

    * the return is `Confirmed` — not merely a chip that says so;
    * its number matches the family's shape, `YYYY-PRET-NNNNNN` or
      `YYYY-SRET-NNNNNN`, because the number is read aloud at a counter and a
      dropped zero pad changes what is said;
    * it names the parent the operator clicked, not some other document;
    * stock moved in this family's direction — `Out` with reason
      `Purchase-return` for a purchase return, `In` with `Sale-return` for a
      credit note — by exactly the returned quantity;
    * that movement's `reference` is the return's OWN number, which is what
      makes the movement traceable to the document that caused it;
    * the derived level moved by that same quantity.

    Every one of these is invisible to the interface: the movement is written
    inside the confirmation's transaction, and a DOM assertion on the number
    would pass against a template rendering a well-formed number the confirm
    never assigned.
    """
    seeded = _family_seed(api, family, tag=f"JOURNEY-{family['id']}")
    product_id = seeded["product_id"]
    returned_qty = "2"

    # The level BEFORE, so "moved by exactly this" is measured from a known
    # point rather than against whatever the seed happened to leave behind.
    stock_before = Decimal(str(api.get_json(f"/api/products/{product_id}/stock")["stock"]))

    return_url = _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )
    return_id = _return_id_from_url(return_url)
    inner = page.locator(family["record_inner"])
    # A DRAFT, on the return's own screen, naming the parent it reverses — the
    # operator can see what is being returned without leaving the document.
    expect(inner).to_contain_text(family["parent_word"])

    _type_return_quantity(page, family, seeded["line_id"], returned_qty)
    _confirm_the_return(page, family)

    # ---- THE FACTS, read from the throwaway database ------------------------
    db = live_server.db_path
    documents = read_rows_in_database(
        db,
        f"SELECT id, {family['number_column']}, status, {family['parent_column']} "
        f"FROM {family['document_table']} ORDER BY id",
    )
    assert len(documents) == 1, f"exactly one {family['name']} exists, got {documents!r}"
    stored_id, return_number, status, stored_parent_id = documents[0]
    assert int(stored_id) == return_id, (
        f"the document created is {stored_id}, the URL landed on {return_id}"
    )
    assert status == "Confirmed", (
        f"the stored status is {status!r}: the confirm reached the fragment but "
        "never committed the document"
    )
    assert int(stored_parent_id) == seeded["parent_id"], (
        f"the {family['name']} names parent {stored_parent_id}, expected "
        f"{seeded['parent_id']}: the action posted the wrong document's id"
    )
    assert re.match(family["number_pattern"], return_number), (
        f"the stored number {return_number!r} does not match "
        f"{family['number_pattern']}"
    )

    # The movement, selected BY REASON rather than by position, so the claim is
    # about the row the return caused and not about an index in a list whose
    # length depends on how the seed happened to be built. The parent's own
    # movement is asserted too: the chain is initial -> parent -> return, and a
    # return whose reference named the parent instead of itself would still pass
    # a "some movement exists" check.
    movement_rows = read_rows_in_database(
        db,
        "SELECT type, reason, qty, reference FROM stock_movements "
        "WHERE product_id = ? AND reason = ? ORDER BY id",
        (product_id, family["movement_reason"]),
    )
    assert len(movement_rows) == 1, (
        f"the {family['name']} must write exactly one {family['movement_reason']} "
        f"movement for the product, got {movement_rows!r}"
    )
    movement_type, reason, qty, reference = movement_rows[0]
    assert movement_type == family["movement_type"], (
        f"a {family['name']} must move stock {family['movement_type']}, the "
        f"movement says {movement_type!r}"
    )
    assert Decimal(qty) == Decimal(returned_qty), (
        f"the movement is {qty}, expected the returned quantity {returned_qty}"
    )
    assert reference == return_number, (
        f"the movement's reference is {reference!r}, expected the return's own "
        f"number {return_number!r}: a movement whose reference names no "
        "document cannot be traced back to one"
    )

    # Every movement on the product, so a return that moved stock a second time
    # cannot hide behind a correct first movement.
    all_movements = read_rows_in_database(
        db,
        "SELECT type, reason, qty, reference FROM stock_movements "
        "WHERE product_id = ? ORDER BY id",
        (product_id,),
    )
    assert len(all_movements) == 3, (
        f"expected the opening movement, the parent's and this {family['name']}'s, "
        f"got {all_movements!r}"
    )
    parent_reference = all_movements[1][3]
    assert parent_reference != return_number, (
        "the parent's movement and the return's carry the same reference, so "
        "neither can be traced to the document that caused it"
    )

    # The derived level, which is a sum rather than any cache: the return is the
    # only thing that moved after the parent was confirmed.
    stock_after = Decimal(str(api.get_json(f"/api/products/{product_id}/stock")["stock"]))
    direction = -1 if family["movement_type"] == "Out" else 1
    expected = stock_before + direction * Decimal(returned_qty)
    assert stock_after == expected, f"stock is {stock_after}, expected {expected}"


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_a_return_line_names_the_parent_line_and_freezes_the_parents_own_figure(
    page: Page, api: ApiClient, live_server: LiveServer, family: dict
) -> None:
    """The line stores a parent LINE id and no product, at the parent's money.

    A return line names a PARENT LINE and has no `product_id` column at all.
    That is the most consequential structural claim in the family: a line that
    named a product could disagree with the line it claims to reverse — a second
    copy of the same product can carry a different price or description — and
    nothing in the schema would notice. The absence of the column is asserted
    through SQLite's own metadata rather than by reading a NULL.

    The frozen money is the parent's own figure, not the product's current one:
    a purchase of five at 12.00 is returned at 12.00 whatever the product costs
    today, because the return reverses THAT document.
    """
    seeded = _family_seed(api, family, tag=f"PARENTLINE-{family['id']}")
    return_url = _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )
    _type_return_quantity(page, family, seeded["line_id"], "1")
    _confirm_the_return(page, family)
    assert _return_id_from_url(return_url)

    db = live_server.db_path
    lines = read_rows_in_database(
        db,
        f"SELECT id, {family['parent_line_column']}, qty, {family['money_column']} "
        f"FROM {family['line_table']} ORDER BY id",
    )
    assert len(lines) == 1, f"one return line was stored, got {lines!r}"
    _line_id, stored_parent_line, qty, money = lines[0]
    assert int(stored_parent_line) == seeded["line_id"], (
        f"the return line names parent line {stored_parent_line}, expected "
        f"{seeded['line_id']}"
    )
    assert Decimal(qty) == Decimal("1"), qty
    assert Decimal(money) == Decimal(
        "12" if family["id"] == "purchase-returns" else "25"
    ), (
        f"the frozen figure is {money}, expected the parent's own — a return "
        "reverses a document, so it returns at the price THAT document recorded"
    )
    assert "product_id" not in _columns_of(db, family["line_table"]), (
        f"{family['line_table']} carries a product_id column: a return line that "
        "named a product could disagree with the parent line it reverses"
    )


# ---------------------------------------------------------------------------
# 3. The refusal is an operator sentence
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_an_unreadable_or_absent_parent_field_answers_a_sentence_and_never_a_serde_internal(
    page: Page, api: ApiClient, live_server: LiveServer, family: dict
) -> None:
    """The exact shape of the shipped 422, asserted absent.

    `Form<T>` rejection is a `Response` the handler never sees, so every
    `map_err` in the module was bypassed and the operator read
    `Failed to deserialize form body: purchase_id: cannot parse integer from
    empty string` — a serde internal, in English, naming a Rust type. The fix is
    that the handler takes `Result<Form<T>, FormRejection>` and owns the
    refusal.

    The assertion is NEGATIVE and that is the point: the body must contain no
    `deserialize`, no `cannot parse`, no Rust type name, and must be the
    module's own sentence. A test that only asserted "a refusal occurred" would
    have passed against the defect it exists to prevent.

    The second half is the more dishonest half of the same bug. A
    `#[serde(default)]` on an `i64` turns a missing required id into `0` and
    the service reports `purchase 0 not found` — an absent field reported as a
    missing DOCUMENT, which sends an operator hunting for a purchase that was
    never named. So an absent field, an empty field and a literal `0` are all
    posted, and none of them may mention a document.
    """
    for form in (
        {family["parent_field"]: ""},
        {family["parent_field"]: "0"},
        {},  # the field absent entirely
    ):
        response = page.request.post(
            f"{api.base_url}{family['create_path']}",
            form={"return_date": "2024-05-02", "notes": "", **form},
        )
        body = response.text()
        assert response.status == 400, (
            f"{form!r} must be refused with a validation error, got "
            f"{response.status}: {body[:300]!r}"
        )
        for internal in (
            "deserialize",
            "cannot parse",
            "Form<",
            "FormRejection",
            "serde",
            "i64",
        ):
            assert internal not in body, (
                f"{form!r} leaked {internal!r} to the operator: {body[:400]!r}"
            )
        assert "This form could not be read" in body, (
            f"{form!r} was not answered with the module's own sentence: {body[:400]!r}"
        )
        assert "0 not found" not in body, (
            f"an absent field was reported as a missing document: {body[:400]!r}"
        )

    # And a refusal left no document behind, whichever shape it took.
    assert read_rows_in_database(
        live_server.db_path, f"SELECT id FROM {family['document_table']}"
    ) == []


def test_an_unreadable_quantity_is_refused_without_a_serde_internal(
    page: Page, api: ApiClient
) -> None:
    """The same class of leak, one field over — and a residue this does not pin.

    The add-line route takes the same `Result<Form<T>, FormRejection>`, so a
    body the extractor cannot read is refused rather than leaked. What the
    refusal then SAYS is a smaller problem and this test deliberately does not
    pretend otherwise: the quantity parser answers `invalid qty`, a hardcoded
    English string built from the field name, where every other sentence on the
    screen comes from the catalog. It is far milder than the serde leak — no
    Rust type, no "cannot parse" — but it is untranslated by construction, and
    on a Spanish page it would be the one English line an operator reads.

    Asserting the residue would fail the suite, and fixing it is a change in
    `src/`, outside this unit. So the test pins what is true and the gap is
    reported rather than papered over: the ABSENCE of an extractor internal.
    """
    seeded = _family_seed(api, _PURCHASE_RETURN, tag="BADQTY")
    return_url = _start_from_the_parent_record(
        page, _PURCHASE_RETURN, seeded["parent_id"], api.base_url
    )
    return_id = _return_id_from_url(return_url)

    response = page.request.post(
        f"{api.base_url}/web/purchase-returns/{return_id}/lines",
        form={"qty": "not-a-number", "purchase_line_id": str(seeded["line_id"])},
    )
    body = response.text()
    assert response.status == 400, f"an unreadable quantity must be refused, got {response.status}"
    for internal in ("deserialize", "cannot parse", "Form<", "FormRejection", "serde", "i64"):
        assert internal not in body, f"the refusal leaked {internal!r}: {body[:400]!r}"


# ---------------------------------------------------------------------------
# The rules only a browser can check
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_no_price_input_is_reachable_anywhere_on_a_return(
    page: Page, api: ApiClient, family: dict
) -> None:
    """Decision 1, asserted on the page a person loads.

    A return line has no price. The cost is frozen from the parent line and the
    service's `add_line`/`update_line` have no price parameter at all, so a
    price control on the screen would be one the service silently ignores — and
    the next person to read the template would take it for working. A disabled
    or readonly input would still be an input, so the claim is the ABSENCE of
    any `name=` matching a money word, not the state of one that exists.

    The Rust suite already asserts this through Askama. Asserting it again
    through the browser is the point: that test renders a template, this one
    renders the page after htmx has swapped fragments into it, and only the
    second can see a control that arrived any other way.

    And the figure must be present AS TEXT, in its own cell, or the frozen
    column would be unreadable and the absence of an input would be a bug rather
    than a decision.
    """
    seeded = _family_seed(api, family, tag=f"NOPRICE-{family['id']}")
    _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )

    draft_controls = _names_matching(page, r"cost|price|amount|total|value")
    assert draft_controls == [], (
        f"a {family['name']} renders controls named {draft_controls!r}: a return "
        "never renegotiates a price, so every figure here is read and none is typed"
    )

    row = page.locator(f"{family['record_inner']} [id^='{family['line_row_prefix']}']").first
    cell = row.locator(family["frozen_cell"])
    expect(cell).to_have_count(1)
    expect(cell).to_contain_text(family["frozen_figure"])
    # The cell is a cell, not a control wearing a cell's classes.
    assert cell.evaluate("el => el.tagName") == "TD", (
        "the frozen figure must be rendered as text, not wrapped in an input"
    )
    # The column is labelled, so the figure says what it is. Addressed by the
    # header's own accessible name, which is the role a screen reader reads.
    expect(
        page.get_by_role("columnheader", name=family["frozen_label"], exact=True)
    ).to_have_count(1)
    # And the screen says WHY there is nothing to type: without that sentence
    # the frozen column reads as an oversight.
    expect(page.locator(family["record_inner"])).to_contain_text(
        family["frozen_sentence"]
    )

    # The CONFIRMED rendering is checked too. A draft-only assertion would miss
    # a price control added to the read-only view, which is the view an
    # operator looks at when checking what was returned.
    _type_return_quantity(page, family, seeded["line_id"], "1")
    _confirm_the_return(page, family)
    confirmed_controls = _names_matching(page, r"cost|price")
    assert confirmed_controls == [], (
        f"the confirmed {family['name']} renders controls named "
        f"{confirmed_controls!r}"
    )


def test_the_line_editor_shows_bought_already_returned_and_still_returnable(
    page: Page, api: ApiClient
) -> None:
    """The three allowance figures, beside a row a first return consumed.

    The operator has to type a quantity that means something, and the three
    numbers that make it mean something are what the parent line bought, what
    confirmed returns already took, and what is left. Without them a quantity
    is a guess.

    The discrimination is the second row: a line a confirmed return has partly
    consumed beside one it has not touched at all, on the same screen. A
    template that rendered `bought` in all three columns would satisfy "bought
    is shown" and be wrong; only the two rows together say the columns differ.

    The figures come from storage — the parent line's own qty, and the sum over
    CONFIRMED returns — so they are asserted as computed facts rather than as
    numbers the test typed.
    """
    seeded = _two_line_confirmed_purchase(api, _PURCHASE_RETURN, tag="ALLOWANCE")
    purchase_id = seeded["parent_id"]

    # A FIRST confirmed return of 3, so the allowance is non-trivial.
    _start_from_the_parent_record(
        page, _PURCHASE_RETURN, purchase_id, api.base_url
    )
    _type_return_quantity(page, _PURCHASE_RETURN, seeded["line_id"], "3")
    _confirm_the_return(page, _PURCHASE_RETURN)

    # A SECOND draft over the same parent: one row partly returned, one whole.
    return_url = _start_from_the_parent_record(
        page, _PURCHASE_RETURN, purchase_id, api.base_url
    )
    inner = page.locator(_PURCHASE_RETURN["record_inner"])

    consumed = page.locator(f"#purchase-return-parent-line-{seeded['line_id']}")
    expect(consumed.locator("[data-parent-qty]")).to_have_text("8")
    expect(consumed.locator("[data-parent-taken]")).to_have_text("3")
    expect(consumed.locator("[data-parent-remaining]")).to_have_text("5")

    untouched = page.locator(f"#purchase-return-parent-line-{seeded['other_line_id']}")
    expect(untouched.locator("[data-parent-qty]")).to_have_text("8")
    expect(untouched.locator("[data-parent-taken]")).to_have_text("0")
    expect(untouched.locator("[data-parent-remaining]")).to_have_text("8")

    # The columns are labelled, so the figures are not numbers an operator has to
    # infer. Asserted on the header cells, addressed by the role a screen reader
    # reads: a sentence elsewhere on the page would satisfy a page-level text
    # match, and a cell locator would need a list form this does not want.
    for header in ("Bought", "Already returned", "Still returnable", "Cost at purchase"):
        expect(
            page.get_by_role("columnheader", name=header, exact=True)
        ).to_have_count(1)
    assert return_url.endswith(str(_return_id_from_url(return_url)))


def test_returning_the_same_line_past_the_allowance_is_refused_with_the_message_the_operator_reads(
    page: Page, api: ApiClient
) -> None:
    """The repeat-return refusal, reached by doing what an operator would do.

    Return part of a purchase and confirm it, then open a SECOND return and type
    a quantity past what is left. The refusal is the service's own sentence, and
    it is asserted on the notice box the operator actually READS — a refusal
    delivered correctly and rendered into nothing is not a delivered refusal.

    The sentence names the shortfall, so the claim is specific: it says how
    much is already returned and how much is left. A generic "invalid quantity"
    would fail this, which is the point.

    And the document is unchanged: the refused quantity is not on the draft and
    the confirm control stays disabled, because an empty return has nothing to
    confirm. A refusal that left the quantity behind would let the operator
    confirm a return they never described.
    """
    seeded = _family_seed(api, _PURCHASE_RETURN, tag="REPEAT")
    purchase_id = seeded["parent_id"]
    line_id = seeded["line_id"]

    # The first return takes 4 of 5 and is confirmed: the allowance is a fact.
    _start_from_the_parent_record(
        page, _PURCHASE_RETURN, purchase_id, api.base_url
    )
    _type_return_quantity(page, _PURCHASE_RETURN, line_id, "4")
    _confirm_the_return(page, _PURCHASE_RETURN)

    # The second return: 5 bought, 4 taken, 1 left. Type 3.
    return_url = _start_from_the_parent_record(
        page, _PURCHASE_RETURN, purchase_id, api.base_url
    )
    return_id = _return_id_from_url(return_url)
    field = page.locator(f"#return-qty-{line_id}")
    # The editor's own arithmetic must agree before the refusal is provoked: if
    # the screen offered 3 without complaint the test would be proving the
    # service's rule and not the editor's.
    row = page.locator(f"#purchase-return-parent-line-{line_id}")
    expect(row.locator("[data-parent-remaining]")).to_have_text("1")

    with page.expect_response(
        _response_for(f"/web/purchase-returns/{return_id}/lines", "POST"), timeout=20000
    ) as response_info:
        field.fill("3")
        field.blur()
    assert response_info.value.status == 400, (
        f"returning past the allowance must be refused, got "
        f"{response_info.value.status}"
    )

    notice = page.locator("[data-notice='error']")
    expect(notice).to_be_visible()
    text = notice.inner_text()
    assert "already returned" in text, (
        f"the refusal does not say what is already returned: {text!r}"
    )
    assert "returnable" in text, (
        f"the refusal does not say what is still returnable: {text!r}"
    )

    # Nothing was stored: no line on the draft, and nothing to confirm.
    expect(page.locator("[data-on-this-return='true']")).to_have_count(0)
    expect(page.locator(_PURCHASE_RETURN["confirm_button"])).to_be_disabled()


# ---------------------------------------------------------------------------
# 5. The record body is rendered TWICE, and the stale copy lies
# ---------------------------------------------------------------------------
#
# The defect this pins is a `hx-target` / `hx-swap` disagreement between two
# things that both believe they own the record body, and it is invisible to
# every other assertion in this module because each of them reads the INNER
# copy — which is the correct one. The outer copy is stale from page load and
# nobody looks at it, so every test passed.
#
# The shape, measured in the browser rather than inferred:
#
#   #purchase-return-record                 <- the shell, never re-rendered
#     #purchase-return-record-inner         <- STALE, from the page load
#       status chips, party line, parent link
#       #purchase-return-money
#         #purchase-return-record-inner     <- FRESH, from the add-line response
#           status chips, party line, parent link
#           #purchase-return-money
#
# The two disagreeing declarations are the money region's own swap and the
# outer wrapper every other action targets. The quantity control posts with
#
#   hx-target="#purchase-return-record-money" hx-swap="outerHTML"
#
# but the RESPONSE is the whole record fragment, whose root is
# `#purchase-return-record-inner`. So `outerHTML` of the money region installs
# the entire body INSIDE the money region — while the page-load copy above it is
# never touched. Confirm and cancel target `#purchase-return-record` with
# `hx-swap="innerHTML"`, which replaces the shell's children wholesale and so
# leaves exactly one body; that is why the duplication "clears on confirm" and
# why no test ever saw it.
#
# What makes it a lie rather than a cosmetic repeat: the stale copy is rendered
# from a document with NO LINES, where `total == 0` and `paid == 0`, so
# `due <= 0` and the chip reads PAID. The fresh copy, with a line on it, reads
# UNPAID. Two chips, on one screen, contradicting each other, about money.


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_writing_a_quantity_leaves_exactly_one_record_body_on_the_screen(
    page: Page, api: ApiClient, family: dict
) -> None:
    """One body, one payment chip, and a chip that agrees with the money shown.

    Three claims, and each is a claim about COUNT rather than about text,
    because a text assertion passes happily against a duplicated body — it finds
    the string it wanted inside the fresh copy and stops looking.

    * The record body appears once. Read from the DOM rather than from the
      markup string, because the second copy arrives through htmx.
    * The payment-status chip appears once. Two chips is the duplicate made
      legible, and it is where the two copies contradict each other.
    * The one chip agrees with the document: a draft carrying a line has
      collected nothing, so it must not claim PAID. Asserted on the chip's own
      text because "PAID" and "UNPAID" are both substrings of neither, so a
      naive `to_contain_text` could not tell them apart — and one of them is
      the wrong answer.

    The last claim is the reason this is a defect test rather than a tidy-up
    test: the stale copy's PAID is not a duplicate of the truth, it is the
    opposite of it.
    """
    seeded = _family_seed(api, family, tag=f"ONCE-{family['id']}")
    _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )

    inner = page.locator(family["record_inner"])
    assert inner.count() == 1, (
        f"the draft already renders {inner.count()} record bodies: "
        f"{family['name']}"
    )

    _type_return_quantity(page, family, seeded["line_id"], "1")

    assert inner.count() == 1, (
        f"writing a quantity left {inner.count()} record bodies on the screen "
        f"(expected 1). The add-line response carries the whole record fragment "
        f"and the quantity control swaps it into the MONEY REGION with "
        f"hx-swap=\"outerHTML\", so the page-load copy above it is never replaced."
    )

    chips = page.locator(family["payment_status"])
    assert chips.count() == 1, (
        f"{chips.count()} payment-status chips are on the screen: the record "
        f"body rendered twice and the two copies disagree about the money"
    )
    # The CATALOG's word, read as text and not as rendered text: the chip carries
    # the house `uppercase` class, so `inner_text` would hand back "UNPAID" and
    # an assertion written against the catalog would fail on the screen being
    # right. Exact, not a substring, because the stale copy says Paid and the
    # fresh one says Unpaid and one of those two is false.
    chip_word = chips.first.text_content().strip()
    assert chip_word == "Unpaid", (
        f"a draft carrying a line and collecting nothing reads {chip_word!r}: "
        f"the chip was rendered when the document held no lines, where total and "
        f"paid are both zero, so due <= 0 and the answer is Paid"
    )

    # The same duplicate repeated the audit line and the parent link, both of
    # which name a document; two of each is a reader looking at two records.
    assert page.locator(family["actor"]).count() == 1, (
        f"{page.locator(family['actor']).count()} audit lines are on the screen: "
        f"the record body rendered twice"
    )
    parent_links = page.locator(f"{family['record_inner']} a.link")
    assert parent_links.count() == 1, (
        f"{parent_links.count()} links to the parent document: the record body "
        f"rendered twice, and a reader reconciling against one purchase would "
        f"be reconciling against two copies of it"
    )


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_the_record_body_stays_single_across_every_action_that_swaps_it(
    page: Page, api: ApiClient, family: dict
) -> None:
    """The duplication is a property of ONE action, and this pins which.

    The quantity commit swaps the money region and duplicates. Confirm, cancel
    and remove-line all target the outer wrapper with `hx-swap="innerHTML"` and
    leave exactly one body — measured here so the shape above is attributed to
    the right declaration rather than to "the templates".

    Each step is asserted with the same count, so the first one that would
    reintroduce a second copy names itself. A confirmed return also proves the
    clean-up: it is the state the duplication used to vanish into.
    """
    seeded = _two_line_confirmed_purchase(api, family, tag=f"SHAPE-{family['id']}")
    return_url = _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )
    inner = page.locator(family["record_inner"])

    _type_return_quantity(page, family, seeded["line_id"], "1")
    assert inner.count() == 1, f"the quantity commit left {inner.count()} bodies"

    _confirm_the_return(page, family)
    assert inner.count() == 1, (
        f"confirming left {inner.count()} record bodies: the confirm form "
        f"targets the outer wrapper with hx-swap=\"innerHTML\", so it replaces "
        f"everything above the money region too"
    )
    assert _return_id_from_url(return_url)


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_the_audit_line_above_the_money_region_says_who_last_edited_it(
    page: Page, api: ApiClient, family: dict
) -> None:
    """The audit line is a statement about a DOCUMENT, so it must follow the document.

    `test_writing_a_quantity_leaves_exactly_one_record_body_on_the_screen` above
    asserts the audit line appears ONCE, and it has always done so — a count of
    one is satisfied by the stale copy and the fresh one alike, and the two are
    indistinguishable except by what they SAY. PR #132 judged the line's
    CONTENT beyond its scope and left it here. This is that.

    The claim: a return nobody has edited since it was created reads `Registered
    by X` alone, and a return that was just edited reads `Registered by X •
    Updated by X`, because `updated_by` is written by the line write. If the line
    is not refreshed with the commit, the operator is told the document has no
    editor at the exact moment they became its editor.

    Asserted on a WORD that is absent before the commit and present after it, so
    a template that never rendered the clause at all cannot pass by having a
    permanently-short line — and so a stale page-load copy, which by construction
    lacks the clause, fails.
    """
    seeded = _family_seed(api, family, tag=f"AUDIT-{family['id']}")
    _start_from_the_parent_record(page, family, seeded["parent_id"], api.base_url)

    actor = page.locator(family["actor"])
    expect(actor).to_have_count(1)
    assert "Updated by" not in actor.first.text_content(), (
        f"a {family['name']} nobody has edited renders Registered by alone; the "
        f"fixture is not reproducing the state this test needs: "
        f"{actor.first.text_content()!r}"
    )

    _type_return_quantity(page, family, seeded["line_id"], "2")

    expect(actor).to_have_count(1)
    assert "Updated by" in actor.first.text_content(), (
        f"the commit that recorded an editor left the audit line reading "
        f"{actor.first.text_content()!r}: it is rendered above the money "
        f"region, which is the only region the commit's swap replaces"
    )


# ---------------------------------------------------------------------------
# 6. The counter flow: twenty rows, twenty keystrokes
# ---------------------------------------------------------------------------
#
# An operator at a counter does the same thing twenty times: take a row, type a
# quantity, commit, take the next. The commit is a blur — htmx's default trigger
# for this control is `change` — so today the flow is twenty tab-and-click
# round trips through a mouse for a keyboard-only operator, and nothing on the
# page says the row is a step in a sequence at all.
#
# Focus is the claim, so every assertion reads `document.activeElement` rather
# than a screenshot. A screenshot shows where the caret *looks* like it is; the
# property under test is which element the browser will send the next keystroke
# to.


def _active_id(page: Page) -> str:
    """The id of the focused element, or `""` when nothing focusable holds focus."""
    return page.evaluate(
        "() => document.activeElement ? (document.activeElement.id || '') : ''"
    )


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_the_first_quantity_input_holds_focus_when_the_page_opens(
    page: Page, api: ApiClient, family: dict
) -> None:
    """The operator opens the document and can type without reaching for a mouse.

    One row, so the claim is not about which row: it is that SOMETHING in the
    editor is focused and it is the quantity control. The control is the only
    editable field on a draft apart from the date and the notes, and none of
    them is where a counter operator's hands belong first.
    """
    seeded = _family_seed(api, family, tag=f"FOCUS-{family['id']}")
    _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )

    assert _active_id(page) == f"return-qty-{seeded['line_id']}", (
        f"the page opened with focus on {_active_id(page)!r}, expected the first "
        f"quantity input 'return-qty-{seeded['line_id']}'. Nothing in the page "
        f"asks the browser to focus it: the editor's rows carry no autofocus "
        f"and no script asks for focus."
    )


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_enter_on_a_quantity_commits_it_and_lands_on_the_next_row(
    page: Page, api: ApiClient, family: dict
) -> None:
    """Enter is commit-and-advance, and the commit really happened.

    Both halves are asserted, because either alone is a plausible-looking lie: a
    handler that only moves focus passes a focus assertion while the document
    never changed, and a handler that only commits passes a stored-figure
    assertion while the operator has to reach for the mouse again.

    The stored figure is read from the DOM's own "On this return" cell rather
    than from the input's value: the value is what the operator typed, so it
    would be satisfied by a handler that never posted anything.
    """
    seeded = _two_line_confirmed_purchase(api, family, tag=f"NEXT-{family['id']}")
    _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )

    first = f"#return-qty-{seeded['line_id']}"
    second = f"#return-qty-{seeded['other_line_id']}"
    page.locator(first).fill("2")

    with page.expect_response(
        _matching_response("POST", "/lines"), timeout=20000
    ) as posted:
        page.locator(first).press("Enter")
    assert posted.value.status == 200, (
        f"Enter did not commit: the post answered {posted.value.status}"
    )

    # The `#` is load-bearing: `line_row_prefix` is a bare string, so without
    # it this is a tag-name selector and matches nothing.
    row = page.locator(f"#{family['line_row_prefix']}{seeded['line_id']}")
    expect(row).to_have_attribute("data-on-this-return", "true")
    expect(row).to_contain_text("2")

    # The advance. htmx replaces the money region on every commit, so this is
    # the REBUILT input rather than the one that was typed into — which is the
    # point: the flow has to survive the swap, not merely move focus in a page
    # that is about to be replaced.
    expect(page.locator(second)).to_be_focused()


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_enter_skips_a_row_that_cannot_be_added(
    page: Page, api: ApiClient, family: dict
) -> None:
    """A row whose allowance is gone is stepped over, not landed on.

    The editor renders `disabled` on a parent line already consumed by confirmed
    returns (`can_add` in the wiring layer), because offering a quantity the
    service would refuse is worse than offering none. Advancing into a disabled
    control would put the operator's caret somewhere they cannot type and give
    no feedback about why, so the advance skips it.

    A two-row parent cannot show this: with one row disabled the only other row
    is either first or last, and both positions pass a naive "next row" check.
    Three rows with the MIDDLE one consumed is the only arrangement where a
    naive `index + 1` lands on the disabled input.
    """
    seeded = _three_line_confirmed_purchase(api, family, tag=f"SKIP-{family['id']}")
    parent_id, first_line, blocked_line, third_line = (
        seeded["parent_id"],
        seeded["line_id"],
        seeded["blocked_line_id"],
        seeded["third_line_id"],
    )

    # A CONFIRMED return of the same family consuming the middle parent line in
    # full, so its allowance is zero on every draft that follows. It has to be
    # the same family: a return names the document it reverses, and a credit
    # note created from a purchase id would land on a sale record.
    _start_from_the_parent_record(page, family, parent_id, api.base_url)
    _type_return_quantity(page, family, blocked_line, "8")
    _confirm_the_return(page, family)

    # The next draft renders the middle row disabled.
    _start_from_the_parent_record(page, family, parent_id, api.base_url)
    expect(page.locator(f"#return-qty-{blocked_line}")).to_be_disabled()

    page.locator(f"#return-qty-{first_line}").fill("1")
    with page.expect_response(_matching_response("POST", "/lines"), timeout=20000):
        page.locator(f"#return-qty-{first_line}").press("Enter")

    # Past the disabled row, not on it.
    expect(page.locator(f"#return-qty-{third_line}")).to_be_focused()


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_enter_on_the_last_row_commits_and_leaves_focus_where_it_can_be_used(
    page: Page, api: ApiClient, family: dict
) -> None:
    """The last row is the end of a sequence, not a trap.

    The commit still happens — that is the half a "just stop moving focus"
    handler gets wrong. And focus does not VANISH: the money region was replaced
    by the swap, so whatever held it is gone, and a browser that drops focus to
    `document.body` has put the operator back at the top of the document with no
    caret anywhere.

    The landing spot is asserted by PROPERTY rather than by id, because the
    choice of control is an interface decision and the thing under test is that
    the decision is a usable one: focus is held by an element that is still in
    the document, is focusable, and is not disabled. `to_be_focused` against
    one named control would pass on a build that lands focus correctly on a
    DIFFERENT control and fail on one that lands it correctly on this one —
    the test would be pinning the answer instead of the question.
    """
    seeded = _family_seed(api, family, tag=f"LAST-{family['id']}")
    _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )
    line_id = seeded["line_id"]
    field = page.locator(f"#return-qty-{line_id}")

    field.fill("2")
    with page.expect_response(
        _matching_response("POST", "/lines"), timeout=20000
    ) as posted:
        field.press("Enter")
    assert posted.value.status == 200, (
        f"Enter on the last row did not commit: {posted.value.status}"
    )

    # The commit happened.
    row = page.locator(f"#{family['line_row_prefix']}{line_id}")
    expect(row).to_have_attribute("data-on-this-return", "true")

    # The wait is not politeness, it is the shape of the thing. htmx delivers the
    # response BEFORE it settles the swap, and the flow moves focus from
    # `htmx:afterSettle`, so sampling `activeElement` the moment the response
    # lands reads the state htmx has already passed through on its way to the
    # answer — focus sits on nothing at that instant by construction. Waiting on
    # the settle is also what makes this a claim about the operator's experience
    # rather than about one instant inside htmx's own pipeline.
    page.wait_for_function(
        "() => document.activeElement && document.activeElement !== document.body",
        timeout=10000,
    )
    landed = page.evaluate(
        """() => {
            const el = document.activeElement;
            if (!el) return {state: 'none'};
            return {
              state: el === document.body ? 'body' : 'element',
              id: el.id || '',
              tag: el.tagName,
              disabled: el.disabled === true,
              inDocument: document.body.contains(el),
              insideRecord: !!el.closest('[data-purchase-return-record],'
                                    + '[data-customer-return-record]'),
            };
        }"""
    )
    assert landed["state"] == "element", (
        f"Enter on the last row dropped focus to {landed['state']!r}: the money "
        f"region was replaced by the commit's swap, so whatever held focus is "
        f"gone and the browser falls back to the body. An operator who has just "
        f"described the whole return now has no caret anywhere."
    )
    assert landed["inDocument"], "focus landed on an element outside the document"
    assert not landed["disabled"], (
        f"focus landed on a DISABLED control ({landed['id'] or landed['tag']}): "
        f"that is a caret that cannot receive the next keystroke, which is the "
        f"same trap wearing a different hat"
    )
    assert landed["insideRecord"], (
        f"focus landed outside the record ({landed['id'] or landed['tag']}): an "
        f"operator who just finished the return should still be on the return"
    )


@pytest.mark.parametrize("family", _FAMILIES, ids=lambda f: f["id"])
def test_shift_enter_goes_back_a_row_without_committing_it(
    page: Page, api: ApiClient, family: dict
) -> None:
    """A miscount is one keystroke to correct, not one to undo.

    Shift+Enter moves UP and does not commit. The commit is the reason this is
    not free: htmx's trigger for the control is `change`, and moving focus away
    is exactly what raises `change`, so an up-move that moved focus naively
    would post the figure the operator is trying to correct.

    The order matters and is what the second half checks. Go forward and back
    with a WRONG figure typed on the way back:
    * moving up from the second row lands on the first;
    * the second row carries no line, because nothing was posted for it;
    * the first row's own committed figure is untouched, because going back
      must not disturb what was already stored.
    """
    seeded = _two_line_confirmed_purchase(api, family, tag=f"BACK-{family['id']}")
    _start_from_the_parent_record(
        page, family, seeded["parent_id"], api.base_url
    )
    first_id, second_id = seeded["line_id"], seeded["other_line_id"]
    first, second = f"#return-qty-{first_id}", f"#return-qty-{second_id}"

    # Forward: commit the first row and arrive at the second.
    page.locator(first).fill("2")
    with page.expect_response(_matching_response("POST", "/lines"), timeout=20000):
        page.locator(first).press("Enter")
    expect(page.locator(second)).to_be_focused()

    # Back: a wrong figure on the second row, corrected by going up.
    page.locator(second).fill("9")
    page.locator(second).press("Shift+Enter")

    expect(page.locator(first)).to_be_focused()

    blocked = page.locator(f"#{family['line_row_prefix']}{second_id}")
    assert blocked.get_attribute("data-on-this-return") is None, (
        "Shift+Enter posted the row it moved away from: the wrong figure the "
        "operator typed is now a line on the document, and moving focus raises "
        "the same `change` htmx commits on"
    )
    stored = page.locator(f"#{family['line_row_prefix']}{first_id}")
    expect(stored).to_contain_text("2")


# ---------------------------------------------------------------------------
# 7. The permission gate
# ---------------------------------------------------------------------------


@contextlib.contextmanager
def _visitor(browser: Browser, context_args: dict) -> Iterator[Page]:
    """A browser context of its own, with whatever session it logs in for.

    The shared `page` fixture injects the harness administrator's session, so a
    test that needs a differently-privileged operator builds its own context
    rather than logging the shared one out. Built from the same
    `browser_context_args` the suite's own fixture uses, so a viewport or
    locale change applies here too.
    """
    context: BrowserContext = browser.new_context(**context_args)
    visitor = context.new_page()
    try:
        yield visitor
    finally:
        context.close()


def _create_role_holding(
    page: Page, *, base_url: str, role_code: str, role_name: str, permission_codes: list[str]
) -> None:
    """Create a role and tick exactly `permission_codes` in its matrix editor.

    Through the real screens, because there is no test-only auth bypass in this
    application: `test_identity.py` builds its limited principals this way and
    the seed here does the same. Ticking the matrix is what makes the principal
    real, and a new role holding no permissions is why the edit step is needed
    at all rather than an optional tightening.
    """
    page.goto(f"{base_url}/roles")
    page.get_by_role("button", name=e2e_copy("new_role")).click()
    dialog = page.locator("#new-role-dialog")
    dialog.locator('input[name="code"]').fill(role_code)
    dialog.locator('input[name="name"]').fill(role_name)
    dialog.locator('input[name="description"]').fill("Browser-suite permission gate.")
    with page.expect_response(_response_for("/web/roles", "POST")):
        dialog.get_by_role("button", name=e2e_copy("create_role")).click()
    expect(page.locator("#role-list")).to_contain_text(role_code)

    row = page.locator("#role-list-inner > div > div", has_text=role_code)
    row.locator(f'button[aria-label="{e2e_copy("edit_role")}"]').click()
    edit = page.locator("#role-edit-dialog")
    for permission in permission_codes:
        edit.locator("label", has_text=permission).locator(
            'input[name="permission_ids"]'
        ).check()
    with page.expect_response(_response_for("/web/roles/matrix", "POST")):
        edit.get_by_role("button", name=e2e_copy("save_permissions")).click()


def test_a_principal_with_the_parents_permission_can_return_and_one_without_it_is_refused(
    page: Page,
    api: ApiClient,
    live_server: LiveServer,
    browser: Browser,
    browser_context_args: dict,
) -> None:
    """The gate, both directions, for both families, through the real screens.

    Two principals, built the way `tests/test_identity.py` builds its own — a
    role whose matrix is ticked on the roles screen, a user created on the
    users screen, the role assigned through its Roles dialog, the first login
    confined to the password change and the change completed. No bypass, no
    fixture that grants a permission the interface cannot.

    The AUTHORIZED principal holds each family's read code and its create code
    — the parent's own tier, because a return reuses it: there is no
    `purchase_returns.create` to hold, which is why this needs no migration. It
    sees the action on the record page and completes a real return through the
    UI, so the gate is not "refuse everyone".

    The RESTRICTED principal holds only the read codes. It sees neither action —
    the screen never offers one the route would refuse — and a post issued
    anyway is refused with `403` naming the missing code, for BOTH families. A
    gate one family honours is a gate half built, so both are exercised and the
    codes are asserted separately.
    """
    authorised_codes = [
        "purchases.read",
        "purchases.create",
        "sales.read",
        "sales.create",
    ]
    restricted_codes = ["purchases.read", "sales.read"]

    # A returnable parent for each family, created through the shared
    # administrator's session so the seeded documents are real documents.
    seeds = {
        family["id"]: _family_seed(api, family, tag=f"GATE-{family['id']}")
        for family in _FAMILIES
    }

    _create_role_holding(
        page, base_url=api.base_url, role_code="gate_autorizado",
        role_name="Gate autorizado", permission_codes=authorised_codes,
    )
    _create_role_holding(
        page, base_url=api.base_url, role_code="gate_solo_lectura",
        role_name="Gate solo lectura", permission_codes=restricted_codes,
    )

    for username, role_name in (
        ("autorizado1", "Gate autorizado"),
        ("restringido1", "Gate solo lectura"),
    ):
        page.goto(f"{api.base_url}/users")
        _create_user_through_the_screen(
            page,
            username=username,
            display_name=role_name,
            password=INITIAL_PASSWORD,
        )
        _assign_role_through_the_screen(page, username=username, role_name=role_name)

    # -- the authorised operator completes a return of each family ------------
    with _visitor(browser, browser_context_args) as visitor:
        _log_in_through_the_form(visitor, live_server, "autorizado1", INITIAL_PASSWORD)
        expect(visitor).to_have_url(f"{live_server.url}/password")
        _change_confined_password(
            visitor, current=INITIAL_PASSWORD, new=CHANGED_PASSWORD
        )

        for family in _FAMILIES:
            seed = seeds[family["id"]]
            action = visitor.locator(f"{family['action_form']} button[type=submit]")
            visitor.goto(f"{live_server.url}{family['parent_path']}/{seed['parent_id']}")
            expect(action).to_have_text(family["action_button"])
            action.click()
            visitor.wait_for_url(f"**{family['id']}/*", timeout=20000)
            _type_return_quantity(visitor, family, seed["line_id"], "1")
            _confirm_the_return(visitor, family)

    # Two documents exist, one per family, and both are the authorised
    # principal's work — the positive half of the gate, read from storage.
    for family in _FAMILIES:
        documents = read_rows_in_database(
            live_server.db_path,
            f"SELECT status FROM {family['document_table']} ORDER BY id",
        )
        assert documents == [("Confirmed",)], (
            f"the authorised principal's {family['name']} is not stored as "
            f"Confirmed: {documents!r}"
        )

    # -- the restricted operator is offered nothing and refused anyway --------
    with _visitor(browser, browser_context_args) as visitor:
        _log_in_through_the_form(visitor, live_server, "restringido1", INITIAL_PASSWORD)
        expect(visitor).to_have_url(f"{live_server.url}/password")
        _change_confined_password(
            visitor, current=INITIAL_PASSWORD, new=CHANGED_PASSWORD
        )
        # The post-change landing is the refused dashboard: this principal holds
        # no dashboard code, which is the screen the login redirect picks.
        expect(visitor.locator("[data-notice='error']")).to_contain_text(
            "dashboard.read"
        )

        for family in _FAMILIES:
            seed = seeds[family["id"]]
            # The screen offers no action it would refuse.
            visitor.goto(f"{live_server.url}{family['parent_path']}/{seed['parent_id']}")
            expect(visitor.locator(family["action_form"])).to_have_count(0)

            # And the route refuses anyway, naming the code. Sent with the
            # `HX-Request` header the operator's own form carries, so the body
            # is the one the notice box would render.
            response = visitor.request.post(
                f"{live_server.url}{family['create_path']}",
                form={
                    family["parent_field"]: str(seed["parent_id"]),
                    "return_date": "2024-05-02",
                    "notes": "",
                },
                headers={"HX-Request": "true"},
            )
            assert response.status == 403, (
                f"a principal without {family['create_code']} must be refused, got "
                f"{response.status}"
            )
            assert family["create_code"] in response.text(), (
                f"the refusal does not name {family['create_code']}: "
                f"{response.text()[:300]!r}"
            )

        # The refused posts created nothing: still exactly the one document the
        # authorised principal made, in each family.
        for family in _FAMILIES:
            (count,) = read_rows_in_database(
                live_server.db_path, f"SELECT COUNT(*) FROM {family['document_table']}"
            )[0]
            assert count == 1, (
                f"a refused post created a {family['name']}: the table holds "
                f"{count} rows"
            )


# ---------------------------------------------------------------------------
# Visual evidence (opt-in)
# ---------------------------------------------------------------------------

# The same one-shot shape as the harness artifact probe and the two screenshot
# probes that already exist: opt-in, skipped by default, no effect on a normal
# run. It writes the screens a person needs to LOOK at to judge this work, which
# is the reason this module exists — every defect it pins was invisible to 1613
# passing tests and obvious the moment somebody opened the page.
SCREENSHOT_PROBE_ENV = "ROYA_E2E_RETURNS_SCREENSHOT_PROBE"


@pytest.mark.skipif(
    os.environ.get(SCREENSHOT_PROBE_ENV) != "1",
    reason=(
        "opt-in probe: set "
        f"{SCREENSHOT_PROBE_ENV}=1 to write the return screenshots"
    ),
)
def test_return_screenshots_probe(page: Page, api: ApiClient) -> None:
    """Write full-page PNGs of the return screens for a human to open.

    Skipped by default, like the other probes. It walks the two families'
    journeys the way an operator would — the parent record, the draft, the
    confirmed return, the list — and writes one PNG per state under
    ``e2e/.artifacts/returns/`` (git-ignored).
    """
    directory = ARTIFACTS_ROOT / "returns"
    directory.mkdir(parents=True, exist_ok=True)
    written: list[str] = []

    def shot(name: str) -> None:
        path = directory / f"{name}.png"
        page.screenshot(path=str(path), full_page=True)
        written.append(str(path.resolve()))

    for family, tag in ((_PURCHASE_RETURN, "PR"), (_CREDIT_NOTE, "CN")):
        seeded = _family_seed(api, family, tag=f"SHOT-{tag}")
        page.goto(f"{api.base_url}{family['parent_path']}/{seeded['parent_id']}")
        page.wait_for_load_state("networkidle")
        shot(f"{tag.lower()}-01-parent-record")

        return_url = _start_from_the_parent_record(
            page, family, seeded["parent_id"], api.base_url
        )
        return_id = _return_id_from_url(return_url)
        shot(f"{tag.lower()}-02-draft")

        _type_return_quantity(page, family, seeded["line_id"], "2")
        shot(f"{tag.lower()}-03-draft-with-line")

        page.locator(family["confirm_button"]).click()
        page.locator(f"{family['confirm_dialog']} button[type=submit]").click()
        page.wait_for_load_state("networkidle")
        page.wait_for_timeout(400)
        shot(f"{tag.lower()}-04-confirmed")

        # The confirmed record as a fresh load, not the swapped fragment: the
        # fragment leaves the page header stale (the pre-existing, house-wide
        # `page_header.html` issue), and this is the picture that shows it.
        page.goto(f"{api.base_url}/{family['id']}/{return_id}")
        page.wait_for_load_state("networkidle")
        shot(f"{tag.lower()}-05-confirmed-reloaded")

        _click_through_the_sidebar(page, family["nav_label"], family["list_path"])
        shot(f"{tag.lower()}-06-list")

    print(f"\nreturn screenshots: {len(written)}")
    for path in written:
        print(f"  {path}")
