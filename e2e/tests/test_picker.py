"""Slice E2: the product picker, driven the way a person uses it.

Two interactions live behind one field, and the interface promises they are
different things:

- a scan resolves an exact barcode and adds the line in a single step, with the
  field coming back empty and focused so the next item can follow immediately;
- choosing from the results adds the product with the quantity already typed.

The second test exists because those are not the same interaction: if the quantity
only travels on the scan path, a mouse user silently loses it. Every assertion
below is something a user can see or a state the API confirms. Nothing waits for a
fixed duration: the search's debounce is waited out by expecting the results, the
way the rest of the suite does.

The seeded draft already carries one line for a *different* product, so "the scan
added a line" is always distinguishable from "the seed did".
"""

from __future__ import annotations

import re
import time
from decimal import Decimal

from playwright.sync_api import Page, expect

from helpers import ApiClient, create_purchase_draft, seed_harness_data

_LINES_TABLE = "#sale-record-money table tbody"
_PURCHASE_LINES_TABLE = "#purchase-record-money table tbody"


def _line_row(page: Page, product_name: str):
    """The lines-table row for a product, addressed by what the row reads."""
    return page.locator(_LINES_TABLE).locator("tr", has_text=product_name)


def _purchase_line_row(page: Page, product_name: str):
    return page.locator(_PURCHASE_LINES_TABLE).locator("tr", has_text=product_name)


def _line_for(api: ApiClient, sale_id: int, product_id: int) -> dict:
    detail = api.get_json(f"/api/sales/{sale_id}")
    return next(line for line in detail["lines"] if int(line["product_id"]) == product_id)


def _purchase_line_for(api: ApiClient, purchase_id: int, product_id: int) -> dict:
    detail = api.get_json(f"/api/purchases/{purchase_id}")
    return next(
        line for line in detail["lines"] if int(line["product_id"]) == product_id
    )


def _wait_for_held(page: Page, held: list, count: int, message: str) -> None:
    """Wait until `count` searches sit paused at the route handler.

    Route interception hands the paused route to the test asynchronously: the
    browser has already issued the request when `expect_request` returns, but
    the Python-side handler may not have appended yet. Poll the list instead of
    asserting immediately or sleeping a fixed duration.
    """
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and len(held) < count:
        page.wait_for_timeout(25)
    assert len(held) == count, f"{message}: {held}"


def test_a_scan_adds_the_line_and_leaves_the_picker_empty_and_focused(
    page: Page, api: ApiClient
) -> None:
    """One interaction: type the barcode, press Enter, and the cart takes it."""
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    expect(picker).to_be_visible()
    # A quantity distinct from the seeded line's 1, so the assertion below cannot
    # pass on the seed's line by accident.
    page.locator("#line-qty").fill("2")

    # The scanner types the barcode and presses Enter. Nothing is clicked.
    picker.fill(data.barcode)
    picker.press("Enter")

    row = _line_row(page, data.product_name)
    expect(row).to_have_count(1)
    expect(row).to_contain_text(re.compile(r"HARNESS-WIDGET\s+2\s+\$"))

    # The point of the test: the field is ready for the next scan without a click.
    # This asserts the real document.activeElement, not the `autofocus` attribute:
    # htmx 1.9 re-focuses the element by id after it swaps the picker, so removing
    # the attribute alone does not break focus (verified in a scratch copy). A test
    # that read the attribute would prove nothing about the next scan.
    expect(picker).to_have_value("")
    expect(picker).to_be_focused()

    line = _line_for(api, data.sale_id, data.product_id)
    assert Decimal(str(line["qty"])) == Decimal("2"), line


def test_the_picker_island_owns_the_sale_search(page: Page, api: ApiClient) -> None:
    """Slice T3: the picker island owns the sale page's client state.

    Three guarantees, in the order an operator meets them:

    - typing reads the island's own JSON route (`/web/product-search.json`)
      and renders the same name / SKU / sale price / stock content the old
      server fragment carried — the field no longer carries any declarative
      htmx read;
    - focus follows the product across a re-search and returns to the field
      when that product leaves the results;
    - clicking a match adds the line with the product id the island
      selected, not the text the field happens to hold: the hidden-id
      defect stays dead because the id travels from island state.
    """
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    results = page.locator("#product-search-results")
    status = page.locator("#product-search-status")
    buttons = results.locator("button")

    # The island owns the search: the field carries no declarative htmx
    # read, so a keystroke cannot be answered by the old HTML fragment route.
    expect(picker).not_to_have_attribute("hx-get", re.compile(r".*"))
    expect(picker).not_to_have_attribute("hx-trigger", re.compile(r".*"))

    # The read is the island's JSON route, and the rendered content is what
    # the operator needs for a sale: name, SKU, sale price and stock.
    with page.expect_request("**/web/product-search.json*") as request_info:
        picker.fill("Harness")
    assert request_info.value.url.endswith("q=Harness")

    expect(buttons).to_have_count(2)
    expect(results).to_contain_text(data.product_name)
    expect(results).to_contain_text("HARNESS-WIDGET • $25.00 • stock 5")
    expect(results).to_contain_text("Harness Spare")
    expect(status).to_have_text("2 matches.")

    # Focus follows the product across a replaced search. The request is held
    # open so real focus can enter the stale results while the search is in
    # flight — the state a fast typist is in.
    held: list = []

    def hold(route) -> None:
        held.append(route)

    page.route("**/web/product-search.json*", hold)

    with page.expect_request("**/web/product-search.json*"):
        picker.fill("Harn")
    _wait_for_held(page, held, 1, "the search request was not held")
    held[0].continue_()
    expect(buttons).to_have_count(2)
    first_product = buttons.nth(0).get_attribute("data-product-id")

    with page.expect_request("**/web/product-search.json*"):
        picker.fill("Harness")
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    _wait_for_held(page, held, 2, "the re-search request was not held")
    held[1].continue_()

    # The same product is still a match, so focus stays on it.
    expect(buttons).to_have_count(2)
    expect(buttons.nth(0)).to_be_focused()
    expect(buttons.nth(0)).to_have_attribute("data-product-id", first_product)
    expect(status).to_have_text("2 matches.")

    # When the focused product leaves the results, focus returns to the field.
    with page.expect_request("**/web/product-search.json*"):
        picker.fill("Widget")
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    expect(buttons.nth(0)).to_contain_text("Harness Spare")
    _wait_for_held(page, held, 3, "the third search was not held")
    held[2].continue_()
    expect(buttons).to_have_count(1)
    expect(picker).to_be_focused()

    # Clicking the match adds the line with the island's selected id and the
    # quantity the operator typed. The field held "Harness Widget"-ish text
    # that is no exact match on its own, so only the island-selected id can
    # have produced this line.
    page.locator("#line-qty").fill("3")
    buttons.nth(0).click()

    row = _line_row(page, data.product_name)
    expect(row).to_have_count(1)
    expect(row).to_contain_text(re.compile(r"HARNESS-WIDGET\s+3\s+\$"))
    line = _line_for(api, data.sale_id, data.product_id)
    assert Decimal(str(line["qty"])) == Decimal("3"), line


def test_choosing_a_result_carries_the_typed_quantity(page: Page, api: ApiClient) -> None:
    """The mouse path adds the product with the quantity the user typed."""
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    picker.fill("Harness")
    # The partial name matches both seeded products; waiting for the matches to
    # appear is how the debounce is waited out, without sleeping past it.
    results = page.locator("#product-search-results")
    expect(results).to_contain_text(data.product_name)
    expect(results).to_contain_text("Harness Spare")

    page.locator("#line-qty").fill("3")
    page.locator("#product-search-results button", has_text=data.product_name).click()

    row = _line_row(page, data.product_name)
    expect(row).to_have_count(1)
    expect(row).to_contain_text(re.compile(r"HARNESS-WIDGET\s+3\s+\$"))

    line = _line_for(api, data.sale_id, data.product_id)
    assert Decimal(str(line["qty"])) == Decimal("3"), line


def test_a_value_with_no_exact_match_is_refused_without_adding_a_line(
    page: Page, api: ApiClient
) -> None:
    """A value nothing resolves exactly is refused, visibly and without blocking."""
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")
    before = api.get_json(f"/api/sales/{data.sale_id}")["lines"]

    picker = page.locator("#product-picker")
    # A partial name that matches several products but resolves to none of them
    # exactly: the user must pick from the results.
    picker.fill("Harness")
    expect(page.locator("#product-search-results")).to_contain_text(data.product_name)

    picker.press("Enter")

    # The message is the app's non-blocking notice, not a browser dialog: it can be
    # dismissed and the page stays usable.
    notice = page.locator("#notice [data-notice='error']")
    expect(notice).to_be_visible()
    # The notice names the action that actually failed, and says why.
    expect(notice).to_contain_text("Add line failed")
    expect(notice).to_contain_text("no exact match")
    expect(notice).to_contain_text("2 matches")
    expect(page.locator("#product-search-results")).to_contain_text(data.product_name)

    after = api.get_json(f"/api/sales/{data.sale_id}")["lines"]
    assert len(after) == len(before), f"the refused value added a line: {after}"


def test_the_results_announce_the_match_count(page: Page, api: ApiClient) -> None:
    """A screen reader is told something happened instead of facing silence."""
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    results = page.locator("#product-search-results")
    # The live region is the semantic under test, so its role is asserted, not the
    # markup that happens to carry it.
    expect(results).to_have_attribute("role", "status")
    expect(results).to_have_attribute("aria-live", "polite")

    status = page.locator("#product-search-status")
    picker = page.locator("#product-picker")

    picker.fill("Harness")
    expect(status).to_have_text("2 matches.")

    picker.fill(data.barcode)
    expect(status).to_have_text("1 match.")

    picker.fill("nothing-matches-this")
    expect(status).to_contain_text("No products match")
    expect(results).to_contain_text("No products match")


def test_a_search_is_not_announced_as_an_added_line(
    page: Page, api: ApiClient
) -> None:
    """The shared notice names the action that actually completed.

    The picker's search read is now the island's own fetch (static/picker.js),
    so it never touches the notice machinery at all; the form post is the only
    request the notice sees, and it must still be announced. (The search used
    to run as an htmx GET inside the add-line form, and the notice used to read
    that form's data-action for any request in it, so every keystroke pause
    announced "Add line saved".)
    """
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    picker.fill("Harness")
    expect(page.locator("#product-search-results")).to_contain_text(data.product_name)
    expect(page.locator("#notice")).not_to_contain_text("Add line saved")
    expect(page.locator("#notice [data-notice]")).to_have_count(0)

    picker.fill(data.barcode)
    picker.press("Enter")
    expect(page.locator("#notice")).to_contain_text("Add line saved")


def test_the_picker_island_owns_the_purchase_search(page: Page, api: ApiClient) -> None:
    """Slice T4: the purchase entry row is the same island, priced for cost.

    Three guarantees, in the order an operator meets them:

    - typing reads the island's own JSON route (`/web/product-search.json`)
      and renders the name / SKU / COST price / stock content the entry row
      needs — the field no longer carries any declarative htmx read;
    - clicking a match adds the line with the product id the island
      selected, not the stale id a pre-filled hidden input could smuggle (the
      hidden-id defect stays dead on this page too);
    - the add response swaps the money region and the entry row comes back
      empty and focused, so the next scan lands without a click.
    """
    data = seed_harness_data(api)
    # The spare product the seed put on the *sale*; the purchase does not own it,
    # so an added line for it below is unambiguously new.
    spare_id = data.sale_line_product_id
    page.goto(f"{api.base_url}/purchases/{data.purchase_id}")

    picker = page.locator("#product-picker")
    results = page.locator("#product-search-results")
    buttons = results.locator("button")

    # The island owns the search here too: the field carries no declarative
    # htmx read, so a keystroke cannot be answered by the old HTML fragment
    # route.
    expect(picker).not_to_have_attribute("hx-get", re.compile(r".*"))
    expect(picker).not_to_have_attribute("hx-trigger", re.compile(r".*"))

    # The read is the island's JSON route, and the rendered content quotes
    # COST — the purchase context's price kind, not the sale's.
    with page.expect_request("**/web/product-search.json*") as request_info:
        picker.fill("Harness")
    assert request_info.value.url.endswith("q=Harness")

    expect(buttons).to_have_count(2)
    expect(results).to_contain_text("Harness Spare")
    expect(results).to_contain_text("cost $")

    # Clicking the match adds the line with the island's selected id and the
    # quantity the operator typed. The field held "Harness" — no exact match
    # for either seeded product — so only the island-selected id can have
    # produced this line; a stale hidden id cannot smuggle one in.
    page.locator("#line-qty").fill("3")
    page.locator("#product-search-results button", has_text="Harness Spare").click()

    row = _purchase_line_row(page, "Harness Spare")
    expect(row).to_have_count(1)
    expect(row.locator("input[name='qty']")).to_have_value("3")
    line = _purchase_line_for(api, data.purchase_id, spare_id)
    assert Decimal(str(line["qty"])) == Decimal("3"), line

    # The entry row is persistent inside the swapped money region, not out of
    # band: the add response re-renders it empty and focused, ready for the
    # next scan.
    expect(picker).to_have_value("")
    expect(picker).to_be_focused()


def test_choosing_a_result_on_the_purchase_page_renders_and_adds(
    page: Page, api: ApiClient
) -> None:
    """The purchase record hosts the same picker, but through its own entry row.

    The sales test above would still pass if only the sale host were fixed, so the
    purchase page needs its own results-based check: its entry row passes a
    different swap target and asks for cost instead of sale price. Here the
    results must render, show the purchase context's cost, and a chosen result
    must add a purchase line with the quantity typed. The seeded purchase already
    owns a line for the other product, so the new row is unambiguous.
    """
    data = seed_harness_data(api)
    # The spare product the seed put on the *sale*; the purchase does not own it.
    spare_id = data.sale_line_product_id
    page.goto(f"{api.base_url}/purchases/{data.purchase_id}")

    # The entry row is persistent: the product field is already on the page,
    # no click reveals it.
    picker = page.locator("#product-picker")
    picker.fill("Harness")
    results = page.locator("#product-search-results")
    expect(results).to_contain_text(data.product_name)
    expect(results).to_contain_text("Harness Spare")
    # The purchase picker quotes cost, not the sale price, so the fragment really
    # travelled through the purchase host's parameters.
    expect(results).to_contain_text("cost $")

    page.locator("#line-qty").fill("4")
    page.locator("#product-search-results button", has_text="Harness Spare").click()

    row = _purchase_line_row(page, "Harness Spare")
    expect(row).to_have_count(1)
    expect(row).to_contain_text("HARNESS-SPARE")
    # Qty and unit cost are inline-edit inputs now (T7): their content is the
    # value attribute, not cell text, so the row shape is asserted through the
    # inputs while the derived subtotal stays plain text.
    expect(row.locator("input[name='qty']")).to_have_value("4")
    expect(row.locator("input[name='unit_cost']")).to_have_value("2.00")
    expect(row).to_contain_text("$8.00")

    detail = api.get_json(f"/api/purchases/{data.purchase_id}")
    line = next(
        line for line in detail["lines"] if int(line["product_id"]) == spare_id
    )
    assert Decimal(str(line["qty"])) == Decimal("4"), line


def test_a_repeat_scan_on_a_draft_purchase_merges_into_one_line(
    page: Page, api: ApiClient
) -> None:
    """Scanning the same barcode twice on a draft purchase increments the line.

    S5b: a repeat product is a legal scan, not an error — when the resolved
    cost equals the existing line's cost (here the supplier's satellite cost,
    the case a receiving desk actually hits) the second scan increments the
    line and the answer says so in the notice region. One row, quantity 2, a
    visible merge notice: nothing happens silently.
    """
    data = seed_harness_data(api)
    # A draft of our own: the seeded purchase already carries a line for this
    # product, and this test needs the increment's before-state to be exact.
    purchase_id = create_purchase_draft(api, data.supplier_id)
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    picker = page.locator("#product-picker")

    # Scan 1: an empty cost resolves to the supplier's satellite cost, so the
    # line is created at it.
    page.locator("#line-qty").fill("1")
    picker.fill(data.barcode)
    picker.press("Enter")
    row = _purchase_line_row(page, data.product_name)
    expect(row).to_have_count(1)
    expect(row.locator("input[name='qty']")).to_have_value("1")

    # Scan 2: the same barcode, the same resolved cost — the line increments.
    page.locator("#line-qty").fill("1")
    picker.fill(data.barcode)
    picker.press("Enter")
    rows = _purchase_line_row(page, data.product_name)
    expect(rows).to_have_count(1)
    expect(rows.locator("input[name='qty']")).to_have_value("2")

    # The merge is announced, not silent: the server-rendered success notice
    # names the product and lands in the page's notice region.
    notice = page.locator("#notice [data-notice='success']")
    expect(notice).to_be_visible()
    expect(notice).to_contain_text(data.product_name)
    expect(notice).to_contain_text("merged")

    detail = api.get_json(f"/api/purchases/{purchase_id}")
    assert len(detail["lines"]) == 1, detail["lines"]
    line = detail["lines"][0]
    assert int(line["product_id"]) == data.product_id
    assert Decimal(str(line["qty"])) == Decimal("2"), line
