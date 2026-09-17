"""Slice E2: the filters on the sales list.

Every criterion narrows the list on its own, two together narrow it further, and a
filter that matches nothing shows the empty state instead of an error. These tests
assert what the list *shows*. Whether the URL preserves the filter is the subject of
slice E3 and is deliberately not asserted here, so this slice cannot pre-empt it.

No fixed sleeps: each change waits for the list fragment's own response, then for
the narrowed list to appear.
"""

from __future__ import annotations

from urllib.parse import urlparse

from playwright.sync_api import Page, expect

from helpers import ApiClient, seed_filter_data

# The rows the list partial renders, addressed from the list element itself so the
# element's own `sale-list-inner` id cannot be mistaken for a row.
_ROWS = '#sale-list-inner > div[id^="sale-"]'
_SALES_PATH = "/web/sales"


def _rows(page: Page):
    return page.locator(_ROWS)


def _row(page: Page, sale_id: int):
    return page.locator(f"#sale-{sale_id}")


def _field(page: Page, name: str):
    return page.locator(f'#sale-filters [name="{name}"]')


def _is_sales_list(response) -> bool:
    return (
        urlparse(response.url).path == _SALES_PATH
        and response.request.method == "GET"
    )


def _open_sales(page: Page, api: ApiClient, expected_rows: int) -> None:
    """Open the list and wait out the re-fetch its `load` trigger fires.

    The page renders the list and htmx immediately asks for it again. Waiting for
    that request matters: otherwise its response can overwrite the first filtered
    result with the unfiltered list.
    """
    with page.expect_response(_is_sales_list):
        page.goto(f"{api.base_url}/sales")
    expect(_rows(page)).to_have_count(expected_rows)


def _apply(page: Page, change) -> None:
    """Apply a filter change and wait for the list request it triggers."""
    with page.expect_response(_is_sales_list):
        change()


def _leave_field(field, value: str) -> None:
    """Type a value, then move on the way a person does.

    The form listens for `change` and for a debounced `keyup`, so setting the value
    alone is not enough: tabbing away is what a user does when the field is done,
    and it is what submits the filter.
    """
    field.fill(value)
    field.press("Tab")


def _set_text(page: Page, name: str, value: str) -> None:
    _apply(page, lambda: _leave_field(_field(page, name), value))


def _choose(page: Page, name: str, value: str) -> None:
    _apply(page, lambda: _field(page, name).select_option(value))


def test_status_filter_narrows_the_list(page: Page, api: ApiClient) -> None:
    data = seed_filter_data(api)
    _open_sales(page, api, expected_rows=3)

    _choose(page, "status", "Confirmed")

    expect(_rows(page)).to_have_count(1)
    expect(_row(page, data.confirmed_id)).to_be_visible()
    expect(_row(page, data.draft_id)).to_have_count(0)
    expect(_row(page, data.cancelled_id)).to_have_count(0)


def test_party_filter_narrows_the_list(page: Page, api: ApiClient) -> None:
    data = seed_filter_data(api)
    _open_sales(page, api, expected_rows=3)

    _set_text(page, "customer", "Alpha")

    # Two sales belong to Filter Alpha, so the list narrows without emptying.
    expect(_rows(page)).to_have_count(2)
    expect(_row(page, data.draft_id)).to_be_visible()
    expect(_row(page, data.confirmed_id)).to_be_visible()
    expect(_row(page, data.cancelled_id)).to_have_count(0)


def test_document_number_filter_narrows_the_list(page: Page, api: ApiClient) -> None:
    data = seed_filter_data(api)
    _open_sales(page, api, expected_rows=3)

    # Only the confirmed sale has a number, and the filter matches a fragment of it
    # because a user remembers a fragment rather than the whole document number.
    fragment = data.confirmed_number.rsplit("-", 1)[1]
    _set_text(page, "number", fragment)

    expect(_rows(page)).to_have_count(1)
    expect(_row(page, data.confirmed_id)).to_be_visible()


def test_date_range_filter_narrows_the_list(page: Page, api: ApiClient) -> None:
    data = seed_filter_data(api)
    _open_sales(page, api, expected_rows=3)

    # Both bounds matter: from the 1st alone keeps two sales, to the 1st alone keeps
    # the other two, and together they keep exactly the June sale.
    _set_text(page, "from", "2024-06-01")
    _set_text(page, "to", "2024-06-01")

    expect(_rows(page)).to_have_count(1)
    expect(_row(page, data.confirmed_id)).to_be_visible()


def test_two_criteria_together_narrow_further(page: Page, api: ApiClient) -> None:
    data = seed_filter_data(api)
    _open_sales(page, api, expected_rows=3)

    _set_text(page, "customer", "Alpha")
    expect(_rows(page)).to_have_count(2)

    _choose(page, "status", "Confirmed")

    expect(_rows(page)).to_have_count(1)
    expect(_row(page, data.confirmed_id)).to_be_visible()
    expect(_row(page, data.draft_id)).to_have_count(0)


def test_a_filter_matching_nothing_shows_the_empty_state(
    page: Page, api: ApiClient
) -> None:
    seed_filter_data(api)
    _open_sales(page, api, expected_rows=3)

    _set_text(page, "customer", "No Such Buyer")

    expect(_rows(page)).to_have_count(0)
    expect(page.locator("#sale-list-inner")).to_contain_text("Nothing here yet.")
    # The empty state is an answer, not an error.
    expect(page.locator("#notice [data-notice='error']")).to_have_count(0)
