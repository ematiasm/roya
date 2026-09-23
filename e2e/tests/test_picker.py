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

    The picker's debounced search request runs inside the add-line form, and the
    notice used to read that form's data-action for any request in it, so every
    keystroke pause announced "Add line saved". A successful search must stay
    silent; the add itself must still be announced.
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
