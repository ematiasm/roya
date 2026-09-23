"""Slice E3: the three defects that motivated the browser suite, closed.

Every test in this file was written **before** its fix and failed against the
committed application; that failure is the evidence that the defect was real and
that the suite can see what the HTTP tests cannot. The three defects are the
user's "clunky buscadores":

1. filtering never updates the URL, so a filtered list cannot be reloaded,
   bookmarked or shared, and Back does not undo a filter change;
2. the results cannot be traversed with the keyboard, so reaching a match means
   tabbing to each button or using a mouse;
3. a search in flight looks exactly like a search that found nothing.

Where the tests interact with the product picker they move **real focus**, the way
a keyboard user does: focus is what makes Enter work on its own and what a screen
reader follows.

No fixed-duration sleeps run the application forward. The one deliberate delay in
this file is Playwright route interception, used to hold a response open long
enough to observe the transient busy state; it is not a sleep in the application
flow and it is commented where it appears.
"""

from __future__ import annotations

from urllib.parse import parse_qs, urlparse

from playwright.sync_api import Page, expect

from helpers import (
    ApiClient,
    add_purchase_line,
    create_purchase_draft,
    create_supplier,
    seed_filter_data,
    seed_harness_data,
)

# ---------------------------------------------------------------------------
# Selectors, copied from the list partials and the picker macro. Tests address
# the interface through the ids it already exposes (R5). Rows are matched by
# their `id="{kind}-{id}"` handle inside the inner list, not by tag: the
# purchase row has been an anchor since S3 (the whole row opens the peek),
# and the sale row is expected to follow in the sales mirror slice, so a
# tag-scoped selector would silently match nothing.
# ---------------------------------------------------------------------------

_SALES_ROWS = '#sale-list-inner > div[id^="sale-"]'
_PURCHASES_ROWS = '#purchase-list-inner > [id^="purchase-"]'
_PRODUCTS_ROWS = '#product-list-inner > div[id^="product-"]'

_SALES_FRAGMENT = "/web/sales"
_PURCHASES_FRAGMENT = "/web/purchases"
_PRODUCTS_FRAGMENT = "/web/products"


def _path(response_url: str) -> str:
    return urlparse(response_url).path


def _query(url: str) -> dict[str, list[str]]:
    return parse_qs(urlparse(url).query)


def _rows(page: Page, selector: str):
    return page.locator(selector)


def _field(page: Page, form_id: str, name: str):
    return page.locator(f"#{form_id} [name='{name}']")


def _is_fragment(path: str, method: str = "GET"):
    def predicate(response) -> bool:
        return response.request.method == method and _path(response.url) == path

    return predicate


def _open_list(page: Page, api: ApiClient, path: str, fragment: str, rows: str, expected: int) -> None:
    """Open a list page and wait out the re-fetch its `load` trigger fires."""
    with page.expect_response(_is_fragment(fragment)):
        page.goto(f"{api.base_url}{path}")
    expect(page.locator(rows)).to_have_count(expected)


# ---------------------------------------------------------------------------
# Defect 1 — filtering must update the URL
# ---------------------------------------------------------------------------


def test_sales_filter_updates_the_url_and_reloading_it_preserves_the_view(
    page: Page, api: ApiClient
) -> None:
    """The addressable page is /sales, not the /web/sales fragment endpoint."""
    data = seed_filter_data(api)
    _open_list(page, api, "/sales", _SALES_FRAGMENT, _SALES_ROWS, expected=3)

    with page.expect_response(_is_fragment(_SALES_FRAGMENT)):
        _field(page, "sale-filters", "status").select_option("Confirmed")
    expect(page.locator(_SALES_ROWS)).to_have_count(1)

    parsed = urlparse(page.url)
    assert parsed.path == "/sales", page.url
    assert _query(page.url).get("status") == ["Confirmed"], page.url

    # Reloading the URL reproduces the filtered view, not the whole list.
    page.reload()
    expect(page.locator("#sale-list-inner")).to_be_visible()
    expect(page.locator(_SALES_ROWS)).to_have_count(1)
    expect(page.locator(f"#sale-{data.confirmed_id}")).to_be_visible()
    expect(_field(page, "sale-filters", "status")).to_have_value("Confirmed")


def test_products_filter_updates_the_url_and_reloading_it_preserves_the_view(
    page: Page, api: ApiClient
) -> None:
    seed_harness_data(api)
    _open_list(page, api, "/products", _PRODUCTS_FRAGMENT, _PRODUCTS_ROWS, expected=2)

    with page.expect_response(_is_fragment(_PRODUCTS_FRAGMENT)):
        field = _field(page, "product-filters", "q")
        field.fill("Widget")
        field.press("Tab")
    expect(page.locator(_PRODUCTS_ROWS)).to_have_count(1)

    parsed = urlparse(page.url)
    assert parsed.path == "/products", page.url
    assert _query(page.url).get("q") == ["Widget"], page.url

    page.reload()
    expect(page.locator(_PRODUCTS_ROWS)).to_have_count(1)
    expect(_field(page, "product-filters", "q")).to_have_value("Widget")


def test_purchases_filter_updates_the_url_and_reloading_it_preserves_the_view(
    page: Page, api: ApiClient
) -> None:
    data = seed_harness_data(api)
    # A second purchase under a different supplier, so the filter discriminates
    # rather than merely re-render the same single row.
    other_supplier = create_supplier(api, "Other Supplier")
    other_purchase = create_purchase_draft(api, other_supplier)
    add_purchase_line(api, other_purchase, data.product_id, qty="1")

    _open_list(page, api, "/purchases", _PURCHASES_FRAGMENT, _PURCHASES_ROWS, expected=2)

    with page.expect_response(_is_fragment(_PURCHASES_FRAGMENT)):
        field = _field(page, "purchase-filters", "supplier")
        field.fill("Harness")
        field.press("Tab")
    expect(page.locator(_PURCHASES_ROWS)).to_have_count(1)

    parsed = urlparse(page.url)
    assert parsed.path == "/purchases", page.url
    assert _query(page.url).get("supplier") == ["Harness"], page.url

    page.reload()
    expect(page.locator(_PURCHASES_ROWS)).to_have_count(1)
    expect(_field(page, "purchase-filters", "supplier")).to_have_value("Harness")


def test_clearing_the_sales_filters_returns_to_the_bare_url(
    page: Page, api: ApiClient
) -> None:
    seed_filter_data(api)
    _open_list(page, api, "/sales", _SALES_FRAGMENT, _SALES_ROWS, expected=3)

    with page.expect_response(_is_fragment(_SALES_FRAGMENT)):
        _field(page, "sale-filters", "status").select_option("Confirmed")
    expect(page.locator(_SALES_ROWS)).to_have_count(1)
    assert _query(page.url).get("status") == ["Confirmed"], page.url

    # Clear is a real link back to the unfiltered page.
    page.locator("#sale-filters a", has_text="Clear").click()

    assert urlparse(page.url).path == "/sales", page.url
    assert urlparse(page.url).query == "", page.url
    expect(page.locator("#sale-list-inner")).to_be_visible()
    expect(page.locator(_SALES_ROWS)).to_have_count(3)


def test_back_after_a_sales_filter_undoes_it(page: Page, api: ApiClient) -> None:
    """Back returns to the previous URL and the view that URL describes.

    Observed against the fixed build: the URL returns to the bare /sales, the list
    returns to all three rows, and the filter field is reset so the form agrees
    with the URL. It is asserted, not assumed.
    """
    seed_filter_data(api)
    _open_list(page, api, "/sales", _SALES_FRAGMENT, _SALES_ROWS, expected=3)

    with page.expect_response(_is_fragment(_SALES_FRAGMENT)):
        _field(page, "sale-filters", "status").select_option("Confirmed")
    expect(page.locator(_SALES_ROWS)).to_have_count(1)

    page.go_back()

    assert urlparse(page.url).path == "/sales", page.url
    assert urlparse(page.url).query == "", page.url
    expect(page.locator(_SALES_ROWS)).to_have_count(3)
    expect(_field(page, "sale-filters", "status")).to_have_value("")


# ---------------------------------------------------------------------------
# Defect 2 — the results must be traversable with the keyboard
# ---------------------------------------------------------------------------


def test_arrow_keys_move_real_focus_through_the_results(
    page: Page, api: ApiClient
) -> None:
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    results = page.locator("#product-search-results")
    buttons = results.locator("button")
    status = page.locator("#product-search-status")

    picker.fill("Harness")
    expect(results).to_contain_text(data.product_name)
    expect(buttons).to_have_count(2)
    expect(status).to_have_text("2 matches.")

    # ArrowDown from the field moves real focus into the first result; the match
    # count keeps being announced while focus moves.
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    expect(status).to_have_text("2 matches.")

    buttons.nth(0).press("ArrowDown")
    expect(buttons.nth(1)).to_be_focused()

    buttons.nth(1).press("ArrowUp")
    expect(buttons.nth(0)).to_be_focused()

    buttons.nth(0).press("ArrowUp")
    expect(picker).to_be_focused()

    # Escape returns to the field and clears it, from a result as well as from
    # the field itself.
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    buttons.nth(0).press("Escape")
    expect(picker).to_be_focused()
    expect(picker).to_have_value("")


def test_enter_on_a_focused_result_adds_that_product(page: Page, api: ApiClient) -> None:
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    results = page.locator("#product-search-results")
    # Results are ordered by name, so Harness Spare is first and Harness Widget
    # second; walking to the second proves the arrow keys really move between
    # controls rather than stopping at the first.
    buttons = results.locator("button")

    page.locator("#line-qty").fill("3")
    picker.fill("Harness")
    expect(results).to_contain_text(data.product_name)
    expect(buttons).to_have_count(2)

    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    buttons.nth(0).press("ArrowDown")
    expect(buttons.nth(1)).to_be_focused()

    # Enter activates the focused result by itself: focus is the selection.
    buttons.nth(1).press("Enter")

    row = page.locator("#sale-record-money table tbody tr", has_text=data.product_name)
    expect(row).to_have_count(1)
    expect(row).to_contain_text("3")

    from decimal import Decimal

    detail = api.get_json(f"/api/sales/{data.sale_id}")
    line = next(
        line for line in detail["lines"] if int(line["product_id"]) == data.product_id
    )
    assert Decimal(str(line["qty"])) == Decimal("3"), line


# ---------------------------------------------------------------------------
# Defect 3 — a search in flight must show that it is working
# ---------------------------------------------------------------------------


def test_an_in_flight_search_shows_a_busy_state(page: Page, api: ApiClient) -> None:
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    busy = page.locator("#product-search-busy")
    results = page.locator("#product-search-results")
    expect(busy).to_be_hidden()

    # A local search answers in milliseconds, so the in-flight state cannot be
    # observed by waiting. Route interception holds the response open: the
    # application flow is untouched, and this is not a sleep in the app.
    held: list = []

    def hold(route) -> None:
        held.append(route)

    page.route("**/web/product-search*", hold)

    # Wait for the request to be issued (and therefore held by the route handler)
    # rather than sleeping: the request is paused, so the interface is in its
    # in-flight state.
    with page.expect_request("**/web/product-search*"):
        page.locator("#product-picker").fill("Harness")

    # The request is now paused at the route handler. The interface must say it
    # is working instead of showing an empty results area.
    expect(busy).to_be_visible()
    expect(busy).to_contain_text("Searching")

    # Release the held response: the busy state goes away and the results arrive.
    assert len(held) == 1, f"the search request was not held: {held}"
    held[0].continue_()

    expect(busy).to_be_hidden()
    expect(results).to_contain_text(data.product_name)



# ---------------------------------------------------------------------------
# Defect 4 — a scan must not be swallowed by a focused result
# ---------------------------------------------------------------------------


def test_a_scan_lands_in_the_picker_while_a_result_is_focused(
    page: Page, api: ApiClient
) -> None:
    """The scanner types into whatever has focus; the interface must catch it.

    A USB reader types the barcode as keystrokes and ends with Enter. If the
    operator left focus on a result, those keystrokes went to the button and the
    scan silently did nothing — the one flow the picker exists for. Nothing is
    focused explicitly here: the scan has to pull focus back on its own.
    """
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    buttons = page.locator("#product-search-results button")

    picker.fill("Harness")
    expect(buttons).to_have_count(2)
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()

    page.keyboard.type(data.barcode)

    # Every character must have landed in the field, not been dropped on the
    # button, and the field must be the element wearing the scan.
    expect(picker).to_have_value(data.barcode)
    expect(picker).to_be_focused()

    page.keyboard.press("Enter")

    expect(picker).to_have_value("")
    row = page.locator("#sale-record-money table tbody tr", has_text=data.product_name)
    expect(row).to_have_count(1)


# ---------------------------------------------------------------------------
# Defect 5 — a pending search must not drop focus to the body
# ---------------------------------------------------------------------------


def _hold_next_search(page: Page) -> list:
    """Pause the next product search, the file's one deliberate delay.

    Route interception holds the response open so the in-flight state can be
    observed (and focus moved during it) without sleeping. The pause handle is
    appended to the returned list; ``held[0].continue_()`` releases it.
    """
    held: list = []
    page.route("**/web/product-search*", lambda route: held.append(route))
    return held


def test_focus_stays_on_the_same_product_across_a_replaced_result(
    page: Page, api: ApiClient
) -> None:
    """The swap replaces the focused button; focus must follow the product.

    The request is held open so focus can enter the results while the search is
    still in flight — the state a fast typist is in.
    """
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    results = page.locator("#product-search-results")
    buttons = results.locator("button")

    # A first search establishes the results the operator can arrow into.
    picker.fill("Harn")
    expect(buttons).to_have_count(2)
    # The focused thing is a product, not a row: compare by the id the results
    # carry, not the row text (whose whitespace depends on who rendered it).
    first_product = buttons.nth(0).get_attribute("data-product-id")

    held = _hold_next_search(page)
    with page.expect_request("**/web/product-search*"):
        picker.fill("Harness")

    # The old results are still on screen while the new search is in flight; move
    # real focus into them, the way a keyboard user does.
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()

    assert len(held) == 1, f"the search request was not held: {held}"
    held[0].continue_()

    # The same product is still a match, so focus must stay on it rather than fall
    # to document.body.
    expect(buttons).to_have_count(2)
    expect(buttons.nth(0)).to_be_focused()
    expect(buttons.nth(0)).to_have_attribute("data-product-id", first_product)
    expect(page.locator("#product-search-status")).to_have_text("2 matches.")

    # Traversal still works from the restored focus: the next ArrowDown moves on to
    # the second result, so the fix did not trap the keyboard on one button.
    buttons.nth(0).press("ArrowDown")
    expect(buttons.nth(1)).to_be_focused()


def test_focus_returns_to_the_picker_when_the_focused_product_disappears(
    page: Page, api: ApiClient
) -> None:
    """When the focused product leaves the results, focus goes back to the field.

    Otherwise focus would sit on a button that no longer exists (or on the body)
    and the next keystroke would go nowhere.
    """
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    results = page.locator("#product-search-results")
    buttons = results.locator("button")

    picker.fill("Harness")
    expect(buttons).to_have_count(2)

    held = _hold_next_search(page)
    with page.expect_request("**/web/product-search*"):
        picker.fill("Widget")

    # Harness Spare is focused; the new query matches only Harness Widget, so the
    # focused product is about to leave the results.
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    expect(buttons.nth(0)).to_contain_text("Harness Spare")

    assert len(held) == 1, f"the search request was not held: {held}"
    held[0].continue_()

    expect(buttons).to_have_count(1)
    expect(picker).to_be_focused()


# ---------------------------------------------------------------------------
# Defect 6 — the stale count must not sit beside the busy announcement
# ---------------------------------------------------------------------------


def test_the_live_region_announces_the_search_instead_of_a_stale_count(
    page: Page, api: ApiClient
) -> None:
    """In flight the region describes the search; after, it carries the count.

    Two polite regions used to coexist, so the results region still held the
    previous number while the busy region said "Searching…". The region a screen
    reader follows must carry one honest state at a time.
    """
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    results = page.locator("#product-search-results")
    status = page.locator("#product-search-status")
    busy = page.locator("#product-search-busy")

    # A first search leaves a count behind, so there is a stale one to be stale.
    picker.fill("Harness")
    expect(status).to_have_text("2 matches.")

    held = _hold_next_search(page)
    with page.expect_request("**/web/product-search*"):
        picker.fill("Harness Spare")

    # In flight: the polite region describes the search, and the count that is
    # about to change is gone from it. The visible cue carries the same words but is
    # hidden from assistive tech, so "Searching…" is announced once.
    expect(busy).to_be_visible()
    expect(busy).to_have_text("Searching…")
    expect(busy).to_have_attribute("aria-hidden", "true")
    expect(status).to_have_text("Searching…")
    expect(results).not_to_contain_text("2 matches")

    assert len(held) == 1, f"the search request was not held: {held}"
    held[0].continue_()

    # After: the same region carries the new count and nothing still says the
    # search is running.
    expect(status).to_have_text("1 match.")
    expect(results).not_to_contain_text("Searching")
    expect(busy).to_be_hidden()

    # Exactly one polite live region, so nothing can announce the same search a
    # second time beside it.
    expect(page.locator("#line-picker [aria-live='polite']")).to_have_count(1)


# ---------------------------------------------------------------------------
# Defect 7 — Space must still activate a focused result
# ---------------------------------------------------------------------------


def test_scan_enter_and_space_keep_their_own_behaviours(
    page: Page, api: ApiClient
) -> None:
    """Three keyboard behaviours, proved together on the real results.

    Redirecting printable characters to the field for the scanner must not take
    Space away from the focused button: Enter and Space both activate the focused
    result, and a scan still lands in the field. The line count is the observable
    effect, so a regression in any one of the three is visible.

    Each behaviour starts from a freshly loaded record page: after a line is added,
    the picker is replaced out of band, and htmx wires that replacement up in a
    settle task that lands just after the swap. Reloading keeps this test about the
    three behaviours rather than about that settle timing.
    """
    data = seed_harness_data(api)
    url = f"{api.base_url}/sales/{data.sale_id}"

    picker = page.locator("#product-picker")
    buttons = page.locator("#product-search-results button")
    rows = page.locator("#sale-record-money table tbody tr")

    # The seed put one Harness Spare line on the draft; each activation below adds
    # one more, so the count proves it happened.
    page.goto(url)
    expect(rows).to_have_count(1)

    # 1. Enter on the focused result adds it.
    picker.fill("Harness")
    expect(buttons).to_have_count(2)
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    buttons.nth(0).press("Enter")
    expect(rows).to_have_count(2)

    # 2. Space on the focused result adds it too: it is a button, and Space is how a
    #    keyboard or screen reader user activates a button.
    page.goto(url)
    expect(rows).to_have_count(2)
    picker.fill("Harness")
    expect(buttons).to_have_count(2)
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    buttons.nth(0).press("Space")
    expect(rows).to_have_count(3)

    # 3. The scan still lands: the focused result does not swallow the characters.
    page.goto(url)
    expect(rows).to_have_count(3)
    picker.fill("Harness")
    expect(buttons).to_have_count(2)
    picker.press("ArrowDown")
    expect(buttons.nth(0)).to_be_focused()
    page.keyboard.type(data.barcode)
    expect(picker).to_have_value(data.barcode)
    expect(picker).to_be_focused()
    page.keyboard.press("Enter")
    expect(rows).to_have_count(4)

    detail = api.get_json(f"/api/sales/{data.sale_id}")
    lines = detail["lines"]
    assert len(lines) == 4, lines
    assert (
        sum(1 for line in lines if int(line["product_id"]) == data.product_id) == 1
    ), lines
    assert (
        sum(
            1
            for line in lines
            if int(line["product_id"]) == data.sale_line_product_id
        )
        == 3
    ), lines


# ---------------------------------------------------------------------------
# Defect 8 — a failed search must not leave the announcement stuck on "Searching…"
# ---------------------------------------------------------------------------


def test_a_failed_search_resets_the_announced_state(
    page: Page, api: ApiClient
) -> None:
    """A failed request must not look like a search that never finishes.

    The single live region carries "Searching…" while the request is in flight
    (the island sets it from state when its fetch goes out). If the request
    fails there is no swap, so the island must replace that text or a screen
    reader is told the search is still running forever.
    """
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")

    picker = page.locator("#product-picker")
    status = page.locator("#product-search-status")
    busy = page.locator("#product-search-busy")

    picker.fill("Harness")
    expect(status).to_have_text("2 matches.")

    def fail(route) -> None:
        route.fulfill(status=500, content_type="text/html", body="search exploded")

    page.route("**/web/product-search*", fail)
    with page.expect_response("**/web/product-search*"):
        picker.fill("Harness Spare")

    # The region says what happened rather than staying on "Searching…", the busy cue
    # is gone, and the global notice still names the failed request.
    expect(status).to_have_text("Search failed.")
    expect(busy).to_be_hidden()
    expect(page.locator("#notice")).to_contain_text("/web/product-search")
