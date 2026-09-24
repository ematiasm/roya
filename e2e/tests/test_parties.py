"""Slice E4: the redesigned customers and suppliers screens.

The redesign turned both pages into a names-only list that opens a slide-over
drawer per record, moved creation into a modal dialog, and added a per-row edit
modal. These tests drive those screens in a real browser and assert what the
browser actually rendered: the list shows the name and nothing the drawer owns,
the drawer lands its header, balance and documents, and the modals create and
edit through the real endpoints.

They deliberately do not re-check balance arithmetic -- the Rust suite owns the
business rules. What is asserted here is UX wiring and rendering: the fragment
arrives, the drawer opens, the value is displayed, the swap lands. The list rows
carry no per-record id, so a row control is addressed by the endpoint it calls;
that both finds the right control and proves the row is bound to the record.

The opt-in probe at the bottom writes full-page screenshots of the six screens
for a human to look at; it is skipped unless ``ROYA_E2E_PARTIES_SCREENSHOT_PROBE=1``.
"""

from __future__ import annotations

import os
from urllib.parse import urlparse

import pytest
from playwright.sync_api import Page, expect

from conftest import ARTIFACTS_ROOT
from helpers import (
    ApiClient,
    account_method_id,
    create_account_with_methods,
    create_confirmed_credit_purchase,
    create_confirmed_credit_sale,
    create_customer,
    create_product,
    create_supplier,
    fund_account,
    record_supplier_cost,
)

# The env gate for the design-screenshot probe, the same one-shot shape as the
# harness's ROYA_E2E_ARTIFACT_PROBE: opt-in, skipped by default, no effect on a
# normal run.
SCREENSHOT_PROBE_ENV = "ROYA_E2E_PARTIES_SCREENSHOT_PROBE"

_CUSTOMERS_PAGE = "/customers"
_CUSTOMERS_LIST = "/web/customers"
_CUSTOMERS_INNER = "customer-list-inner"
_SUPPLIERS_PAGE = "/suppliers"
_SUPPLIERS_LIST = "/web/suppliers"
_SUPPLIERS_INNER = "supplier-list-inner"


# ---------------------------------------------------------------------------
# Small navigation helpers
# ---------------------------------------------------------------------------


def _response_for(path: str, method: str = "GET"):
    """Predicate matching one request path and method, query string ignored."""

    def matches(response) -> bool:
        return urlparse(response.url).path == path and response.request.method == method

    return matches


def _row_button(page: Page, inner_id: str, hx_get: str):
    """The row control wired to one record, addressed by its ``hx-get``.

    The redesigned rows carry no per-record id, so the endpoint the control calls
    is the stable key: it finds the right button and proves the row is bound to
    the record under test.
    """
    return page.locator(f'#{inner_id} button[hx-get="{hx_get}"]')


def _open_names_list(
    page: Page, api: ApiClient, *, page_path: str, list_path: str, inner_id: str, name: str
) -> None:
    """Open a parties page and wait out the re-fetch its ``load`` trigger fires.

    The server renders the list and htmx immediately asks for it again; acting
    before that second response lands risks the swap replacing the row mid-click.
    """
    with page.expect_response(_response_for(list_path)):
        page.goto(f"{api.base_url}{page_path}")
    expect(page.locator(f"#{inner_id}")).to_contain_text(name)


def _open_drawer(page: Page, inner_id: str, path: str) -> None:
    """Click a row name and wait for its detail fragment before asserting.

    The name's ``hx-get`` and the fragment path are the same, so one argument
    both finds the control and synchronizes on its response.
    """
    with page.expect_response(_response_for(path)):
        _row_button(page, inner_id, path).click()


# ---------------------------------------------------------------------------
# Shared seeds
# ---------------------------------------------------------------------------


def _seeded_product(api: ApiClient, sku: str, name: str) -> int:
    """A tracked product with enough stock for a confirm to deduct from."""
    product = create_product(
        api,
        sku=sku,
        name=name,
        sale_price="25.00",
        cost_price="10.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    return int(product["id"])


# ---------------------------------------------------------------------------
# Customers
# ---------------------------------------------------------------------------


def test_customers_list_shows_names_only(page: Page, api: ApiClient) -> None:
    """The list renders the name, not the fields the drawer owns.

    Seeded with a phone, a credit limit and a payment term, so any of the old
    badges, ageing buckets, limit or phone that leaked back into the list would
    show here. The structural check is the sharpest one: one row is exactly two
    buttons (the name and the edit) and no form, where the old row carried a
    Statement link and Deactivate/Delete forms.
    """
    customer_id = create_customer(
        api,
        "List Buyer One",
        phone="555-0199",
        credit_limit="500.00",
        due_days=30,
    )

    _open_names_list(
        page,
        api,
        page_path=_CUSTOMERS_PAGE,
        list_path=_CUSTOMERS_LIST,
        inner_id=_CUSTOMERS_INNER,
        name="List Buyer One",
    )

    listing = page.locator(f"#{_CUSTOMERS_INNER}")
    expect(listing).to_contain_text("List Buyer One")
    for forbidden in ("555-0199", "ageing", "limit", "walk-in", "Statement", "Deactivate"):
        expect(listing).not_to_contain_text(forbidden)
    # One row, two controls, no form: the row has no actions inline anymore. The
    # walk-in the migrations seed is another row, so scope the count to this one.
    row = _row_button(
        page, _CUSTOMERS_INNER, f"/web/customers/detail/{customer_id}"
    ).locator("xpath=..")
    expect(row.locator("button")).to_have_count(2)
    expect(row.locator("form")).to_have_count(0)


def test_clicking_a_customer_name_opens_the_drawer_with_statement(
    page: Page, api: ApiClient
) -> None:
    """The name opens the right-hand drawer with header, balance and documents.

    Asserts the drawer rendered content, not that a request fired: the header
    carries the phone/limit/term the list hides, the balance line renders, and
    the Confirmed credit sale seeded for this customer shows as a receivable
    document with its assigned number.
    """
    product_id = _seeded_product(api, "PARTY-CUST", "Party Customer Product")
    customer_id = create_customer(
        api,
        "Drawer Buyer",
        phone="555-0142",
        credit_limit="500.00",
        due_days=30,
    )
    sale_id = create_confirmed_credit_sale(
        api, customer_id, product_id, qty="1", unit_price="10.00"
    )
    sale_number = api.get_json(f"/api/sales/{sale_id}")["sale"]["sale_number"]

    _open_names_list(
        page,
        api,
        page_path=_CUSTOMERS_PAGE,
        list_path=_CUSTOMERS_LIST,
        inner_id=_CUSTOMERS_INNER,
        name="Drawer Buyer",
    )
    _open_drawer(
        page,
        _CUSTOMERS_INNER,
        f"/web/customers/detail/{customer_id}",
    )

    expect(page.locator("#customer-drawer")).to_be_visible()
    body = page.locator("#customer-detail-inner")
    expect(body).to_contain_text("Drawer Buyer")
    expect(body).to_contain_text("555-0142")
    expect(body).to_contain_text("limit 500.00")
    expect(body).to_contain_text("term 30d")
    expect(page.locator("#customer-statement-inner .text-2xl")).to_contain_text(
        "balance as of"
    )
    expect(page.locator("#customer-statement-inner .text-2xl")).to_contain_text("10.00")
    expect(body).to_contain_text("Receivable sales (1)")
    expect(body).to_contain_text(str(sale_number))


def test_new_customer_modal_creates_and_refreshes_the_list(
    page: Page, api: ApiClient
) -> None:
    """The New customer button opens the modal; submitting creates the record.

    The created name must land in the list next to the existing one, proving the
    create POST answered with the refreshed list and the ``customer-created``
    event closed the modal.
    """
    create_customer(api, "Existing Buyer")
    _open_names_list(
        page,
        api,
        page_path=_CUSTOMERS_PAGE,
        list_path=_CUSTOMERS_LIST,
        inner_id=_CUSTOMERS_INNER,
        name="Existing Buyer",
    )

    page.get_by_role("button", name="New customer").click()
    dialog = page.locator("#new-customer-dialog")
    expect(dialog).to_be_visible()
    dialog.locator('input[name="name"]').fill("Modal Buyer")
    dialog.locator('input[name="phone"]').fill("555-0177")
    dialog.get_by_role("button", name="Create Customer").click()

    listing = page.locator(f"#{_CUSTOMERS_INNER}")
    expect(listing).to_contain_text("Modal Buyer")
    expect(listing).to_contain_text("Existing Buyer")
    expect(dialog).not_to_be_visible()


def test_customer_edit_modal_is_prefilled_and_saves(page: Page, api: ApiClient) -> None:
    """The row edit opens a prefilled modal; saving updates the row.

    Prefill is the point: the list row carries only the name, so the modal must
    fetch the entity's current data. Assert the fields hold the current values
    before editing, then that the change lands in the refreshed list.
    """
    customer_id = create_customer(
        api,
        "Prefill Buyer",
        phone="555-0120",
        credit_limit="250.00",
        due_days=15,
    )
    _open_names_list(
        page,
        api,
        page_path=_CUSTOMERS_PAGE,
        list_path=_CUSTOMERS_LIST,
        inner_id=_CUSTOMERS_INNER,
        name="Prefill Buyer",
    )

    with page.expect_response(
        _response_for(f"/web/customers/edit-form/{customer_id}")
    ):
        _row_button(
            page, _CUSTOMERS_INNER, f"/web/customers/edit-form/{customer_id}"
        ).click()

    dialog = page.locator("#customer-edit-dialog")
    expect(dialog).to_be_visible()
    expect(dialog.locator('input[name="name"]')).to_have_value("Prefill Buyer")
    expect(dialog.locator('input[name="phone"]')).to_have_value("555-0120")
    expect(dialog.locator('input[name="credit_limit"]')).to_have_value("250.00")
    dialog.locator('input[name="name"]').fill("Edited Buyer")
    dialog.get_by_role("button", name="Save Changes").click()

    listing = page.locator(f"#{_CUSTOMERS_INNER}")
    expect(listing).to_contain_text("Edited Buyer")
    expect(listing).not_to_contain_text("Prefill Buyer")
    expect(dialog).not_to_be_visible()


# ---------------------------------------------------------------------------
# Suppliers
# ---------------------------------------------------------------------------


def test_suppliers_list_shows_names_only(page: Page, api: ApiClient) -> None:
    """The list renders the name, not the costs, phone or notes the drawer owns.

    The supplier is seeded with a recorded product cost, a phone and notes: all
    three were columns of the old list, so any leak would show. One row is again
    exactly two buttons and no form.
    """
    product_id = _seeded_product(api, "PARTY-SUP", "Supplier List Product")
    supplier_id = create_supplier(
        api,
        "List Supplier One",
        phone="555-0188",
        notes="net 30 supplier",
    )
    record_supplier_cost(api, product_id, supplier_id, cost="9.50")

    _open_names_list(
        page,
        api,
        page_path=_SUPPLIERS_PAGE,
        list_path=_SUPPLIERS_LIST,
        inner_id=_SUPPLIERS_INNER,
        name="List Supplier One",
    )

    listing = page.locator(f"#{_SUPPLIERS_INNER}")
    expect(listing).to_contain_text("List Supplier One")
    for forbidden in (
        "555-0188",
        "net 30 supplier",
        "Supplier List Product",
        "Costs (",
        "Deactivate",
    ):
        expect(listing).not_to_contain_text(forbidden)
    row = _row_button(
        page, _SUPPLIERS_INNER, f"/web/suppliers/{supplier_id}/detail"
    ).locator("xpath=..")
    expect(row.locator("button")).to_have_count(2)
    expect(row.locator("form")).to_have_count(0)


def test_clicking_a_supplier_name_opens_the_drawer_with_balance_and_purchases(
    page: Page, api: ApiClient
) -> None:
    """The name opens the drawer with header, outstanding balance and purchases.

    The Confirmed credit purchase seeded here (3 x 10.00) gives the drawer a real
    payable, so the balance line must render it and the purchase must appear as
    a document linking to its record. The browser proves the value is displayed,
    not that the arithmetic is right.
    """
    product_id = _seeded_product(api, "PARTY-DRAW", "Party Drawer Product")
    supplier_id = create_supplier(
        api,
        "Drawer Supplier",
        phone="555-0166",
        notes="drawer notes",
    )
    purchase_id = create_confirmed_credit_purchase(
        api, supplier_id, product_id, qty="3", unit_cost="10.00"
    )
    purchase_number = api.get_json(f"/api/purchases/{purchase_id}")["purchase"][
        "purchase_number"
    ]

    _open_names_list(
        page,
        api,
        page_path=_SUPPLIERS_PAGE,
        list_path=_SUPPLIERS_LIST,
        inner_id=_SUPPLIERS_INNER,
        name="Drawer Supplier",
    )
    _open_drawer(
        page,
        _SUPPLIERS_INNER,
        f"/web/suppliers/{supplier_id}/detail",
    )

    expect(page.locator("#supplier-drawer")).to_be_visible()
    body = page.locator("#supplier-detail-inner")
    expect(body).to_contain_text("Drawer Supplier")
    expect(body).to_contain_text("555-0166")
    expect(body).to_contain_text("drawer notes")
    expect(page.locator("#supplier-detail-inner .text-2xl")).to_contain_text(
        "outstanding across Confirmed purchases"
    )
    expect(page.locator("#supplier-detail-inner .text-2xl")).to_contain_text("30.00")
    expect(body).to_contain_text("Purchases (1)")
    expect(body).to_contain_text(str(purchase_number))
    # The purchase renders as a document link to its record.
    expect(body.locator(f'a[href="/purchases/{purchase_id}"]')).to_be_visible()


def test_new_supplier_modal_creates_and_refreshes_the_list(
    page: Page, api: ApiClient
) -> None:
    """The New supplier button opens the modal; submitting creates the record."""
    create_supplier(api, "Existing Supplier")
    _open_names_list(
        page,
        api,
        page_path=_SUPPLIERS_PAGE,
        list_path=_SUPPLIERS_LIST,
        inner_id=_SUPPLIERS_INNER,
        name="Existing Supplier",
    )

    page.get_by_role("button", name="New supplier").click()
    dialog = page.locator("#new-supplier-dialog")
    expect(dialog).to_be_visible()
    dialog.locator('input[name="name"]').fill("Modal Supplier")
    dialog.locator('input[name="phone"]').fill("555-0155")
    dialog.get_by_role("button", name="Create Supplier").click()

    listing = page.locator(f"#{_SUPPLIERS_INNER}")
    expect(listing).to_contain_text("Modal Supplier")
    expect(listing).to_contain_text("Existing Supplier")
    expect(dialog).not_to_be_visible()


def test_supplier_edit_modal_is_prefilled_and_saves(page: Page, api: ApiClient) -> None:
    """The row edit opens a prefilled modal; saving updates the row."""
    supplier_id = create_supplier(
        api,
        "Prefill Supplier",
        phone="555-0133",
        notes="old notes",
    )
    _open_names_list(
        page,
        api,
        page_path=_SUPPLIERS_PAGE,
        list_path=_SUPPLIERS_LIST,
        inner_id=_SUPPLIERS_INNER,
        name="Prefill Supplier",
    )

    with page.expect_response(_response_for(f"/web/suppliers/{supplier_id}/edit-form")):
        _row_button(
            page, _SUPPLIERS_INNER, f"/web/suppliers/{supplier_id}/edit-form"
        ).click()

    dialog = page.locator("#supplier-edit-dialog")
    expect(dialog).to_be_visible()
    expect(dialog.locator('input[name="name"]')).to_have_value("Prefill Supplier")
    expect(dialog.locator('input[name="phone"]')).to_have_value("555-0133")
    expect(dialog.locator('input[name="notes"]')).to_have_value("old notes")
    dialog.locator('input[name="name"]').fill("Edited Supplier")
    dialog.get_by_role("button", name="Save Changes").click()

    listing = page.locator(f"#{_SUPPLIERS_INNER}")
    expect(listing).to_contain_text("Edited Supplier")
    expect(listing).not_to_contain_text("Prefill Supplier")
    expect(dialog).not_to_be_visible()


def test_paying_a_supplier_from_the_drawer_reduces_the_balance(
    page: Page, api: ApiClient
) -> None:
    """The Pay supplier card swaps the drawer's balance in place.

    The browser proves the wiring, not the arithmetic: the seeded Confirmed
    credit purchase shows its outstanding balance, and paying part of it must
    make the displayed balance drop to the remainder. The account is funded
    because paying posts an Expense and overdraft is blocked by default.
    """
    account_id = create_account_with_methods(api, "Caja", ("Cash",))
    fund_account(api, account_id, "100.00")
    method_id = account_method_id(api, account_id, "Cash")

    product_id = _seeded_product(api, "PARTY-PAY", "Party Pay Product")
    supplier_id = create_supplier(api, "Paid Supplier")
    create_confirmed_credit_purchase(
        api, supplier_id, product_id, qty="3", unit_cost="10.00"
    )

    _open_names_list(
        page,
        api,
        page_path=_SUPPLIERS_PAGE,
        list_path=_SUPPLIERS_LIST,
        inner_id=_SUPPLIERS_INNER,
        name="Paid Supplier",
    )
    _open_drawer(
        page,
        _SUPPLIERS_INNER,
        f"/web/suppliers/{supplier_id}/detail",
    )

    balance = page.locator("#supplier-detail-inner .text-2xl")
    expect(balance).to_contain_text("30.00")

    pay_form = page.locator(
        '#supplier-detail-inner form[hx-post="/web/supplier-payments"]'
    )
    pay_form.locator('input[name="amount"]').fill("10.00")
    # The option value is the method id; selecting by value avoids matching the
    # label text that duplicates across accounts.
    pay_form.locator('select[name="method_id"]').select_option(str(method_id))
    with page.expect_response(_response_for("/web/supplier-payments", "POST")):
        pay_form.get_by_role("button", name="Pay supplier").click()

    # The drawer body was swapped: re-resolve the balance and watch it drop.
    expect(page.locator("#supplier-detail-inner .text-2xl")).to_contain_text("20.00")
    expect(page.locator("#supplier-detail-inner .text-2xl")).not_to_contain_text("30.00")


# ---------------------------------------------------------------------------
# Visual evidence (opt-in)
# ---------------------------------------------------------------------------


@pytest.mark.skipif(
    os.environ.get(SCREENSHOT_PROBE_ENV) != "1",
    reason=(
        "opt-in probe: set "
        f"{SCREENSHOT_PROBE_ENV}=1 to write the design screenshots"
    ),
)
def test_design_screenshots_probe(page: Page, api: ApiClient) -> None:
    """Write full-page screenshots of the redesigned screens for a human to open.

    Skipped by default, like the harness artifact probe. It seeds a shop with a
    Confirmed credit sale and a Confirmed credit purchase so each drawer has real
    content, then walks the six screens and writes one PNG per state under
    ``e2e/.artifacts/design/`` (git-ignored).
    """
    product_id = _seeded_product(api, "SHOT-WIDGET", "Screenshot Widget")
    # An account that owns a method, so the Collect and Pay supplier cards show a
    # real option instead of an empty select. Without it the screenshots would
    # depict a shop with no usable payment method, which is not the design under
    # review.
    account_id = create_account_with_methods(api, "Caja", ("Cash",))
    fund_account(api, account_id, "100.00")
    customer_id = create_customer(
        api,
        "Ana Screenshot",
        phone="555-0101",
        credit_limit="500.00",
        due_days=30,
    )
    create_confirmed_credit_sale(
        api, customer_id, product_id, qty="1", unit_price="10.00"
    )
    supplier_id = create_supplier(
        api,
        "Distribuidora Sur",
        phone="11 5555-5555",
        notes="Entregas los martes",
    )
    create_confirmed_credit_purchase(
        api, supplier_id, product_id, qty="3", unit_cost="10.00"
    )

    directory = ARTIFACTS_ROOT / "design"
    directory.mkdir(parents=True, exist_ok=True)
    written: list[str] = []

    def shot(name: str) -> None:
        path = directory / name
        page.screenshot(path=str(path), full_page=True)
        written.append(str(path.resolve()))

    # 1. Customers list
    _open_names_list(
        page,
        api,
        page_path=_CUSTOMERS_PAGE,
        list_path=_CUSTOMERS_LIST,
        inner_id=_CUSTOMERS_INNER,
        name="Ana Screenshot",
    )
    shot("01-customers-list.png")

    # 2. Customer drawer open
    _open_drawer(
        page,
        _CUSTOMERS_INNER,
        f"/web/customers/detail/{customer_id}",
    )
    expect(page.locator("#customer-drawer")).to_be_visible()
    expect(page.locator("#customer-detail-inner")).to_contain_text("Ana Screenshot")
    shot("02-customer-drawer.png")

    # 3. Customer create modal (drawer closed first for a clean shot)
    page.locator('#customer-drawer button[aria-label="Close detail"]').click()
    page.get_by_role("button", name="New customer").click()
    expect(page.locator("#new-customer-dialog")).to_be_visible()
    shot("03-customer-create-modal.png")
    page.locator('#new-customer-dialog button[type="button"]').first.click()

    # 4. Suppliers list
    _open_names_list(
        page,
        api,
        page_path=_SUPPLIERS_PAGE,
        list_path=_SUPPLIERS_LIST,
        inner_id=_SUPPLIERS_INNER,
        name="Distribuidora Sur",
    )
    shot("04-suppliers-list.png")

    # 5. Supplier drawer open
    _open_drawer(
        page,
        _SUPPLIERS_INNER,
        f"/web/suppliers/{supplier_id}/detail",
    )
    expect(page.locator("#supplier-drawer")).to_be_visible()
    expect(page.locator("#supplier-detail-inner")).to_contain_text("Distribuidora Sur")
    shot("05-supplier-drawer.png")

    # 6. Supplier edit modal. The open drawer covers the row controls (that is
    # what a slide-over does), so close it first, as a person would.
    page.locator('#supplier-drawer button[aria-label="Close detail"]').click()
    with page.expect_response(_response_for(f"/web/suppliers/{supplier_id}/edit-form")):
        _row_button(
            page, _SUPPLIERS_INNER, f"/web/suppliers/{supplier_id}/edit-form"
        ).click()
    expect(page.locator("#supplier-edit-dialog")).to_be_visible()
    shot("06-supplier-edit-modal.png")

    for path in written:
        print(f"screenshot: {path}")
