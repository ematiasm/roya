"""The documents drawer, proven from the browser's side.

The Rust suite owns the drawer route's decisions — narrowing, action
permissions, impact-preview text, the draft delete's guard. These tests prove
what only a real browser can: the drawer opens on a click and closes on
Escape, closing empties the body's DOM, the open never touches the URL nor
reloads the page, the impact preview stands above the button before it is
pressed, a confirmed delete re-reads the feed and closes the drawer without a
navigation, and the feed a narrowed principal sees shows only its family —
rendered, clickable rows included, not an attribute a Rust test could read.

Each test seeds its own documents, so none depends on another's leftovers.
"""

from __future__ import annotations

from urllib.parse import urlparse

import pytest
from playwright.sync_api import Page, expect

from helpers import (
    ApiClient,
    account_method_id,
    add_sale_line,
    create_account_with_methods,
    create_confirmed_credit_purchase,
    create_confirmed_credit_sale,
    create_customer,
    create_product,
    create_sale_draft,
    create_supplier,
    e2e_copy,
)

_DOCUMENTS_PAGE = "/documents"
_DOCUMENT_LIST = "#document-list"
_DRAWER = "#document-drawer"
_DRAWER_BODY = "#document-drawer-body"
_DETAIL_INNER = "#document-detail-inner"

# The initial credential the users screen assigns, and the one the confined
# session changes to — the same pair the identity tests use (both ≥ 12 chars,
# the service's minimum).
INITIAL_PASSWORD = "first-password-123"
CHANGED_PASSWORD = "changed-password-456"


# ---------------------------------------------------------------------------
# Small navigation helpers (the parties drawer pattern)
# ---------------------------------------------------------------------------


def _response_for(path: str, method: str = "GET"):
    """Predicate matching one request path and method, query string ignored."""

    def matches(response) -> bool:
        return urlparse(response.url).path == path and response.request.method == method

    return matches


def _open_documents_page(page: Page, api: ApiClient) -> None:
    """Open `/documents` and wait out the re-fetch its ``load`` trigger fires.

    The server renders the list and htmx immediately asks for it again; acting
    before that second response lands risks the swap replacing the row
    mid-click.
    """
    with page.expect_response(_response_for("/web/documents")):
        page.goto(f"{api.base_url}{_DOCUMENTS_PAGE}")
    expect(page.locator(f"{_DOCUMENT_LIST} #document-list-inner")).to_be_visible()


def _open_drawer(page: Page, kind: str, document_id: int) -> None:
    """Click a row's identifier and wait for its detail fragment.

    The identifier's ``hx-get`` and the fragment path are the same, so one
    path both finds the control and synchronizes on its response.
    """
    path = f"/web/documents/detail/{kind}/{document_id}"
    with page.expect_response(_response_for(path)):
        page.locator(f'{_DOCUMENT_LIST} button[hx-get="{path}"]').click()
    expect(page.locator(_DRAWER)).to_be_visible()
    expect(page.locator(_DETAIL_INNER)).to_be_visible()


def _answer_next_dialog(page: Page, *, accept: bool) -> list[str]:
    """Answer the next dialog the moment it opens, and record what it asked.

    The confirmation is a native dialog raised inside the click handler, so the
    page's JavaScript is blocked until it is answered. A persistent handler is
    what lets the click finish; ``page.expect_event`` alone would deadlock the
    click.
    """
    seen: list[str] = []

    def handler(dialog) -> None:
        seen.append(dialog.message)
        if accept:
            dialog.accept()
        else:
            dialog.dismiss()

    page.on("dialog", handler)
    return seen


# ---------------------------------------------------------------------------
# Case 6's principal: built through the real screens, the way the identity
# tests build theirs (a role whose matrix holds exactly one read code, a user
# holding that role, and the confined password change).
# ---------------------------------------------------------------------------


def _create_role_with_one_permission(
    page: Page, *, base_url: str, code: str, role_name: str
) -> None:
    """A role whose permission matrix holds exactly one code, via the editor."""
    page.goto(f"{base_url}/roles")
    page.get_by_role("button", name=e2e_copy("new_role")).click()
    dialog = page.locator("#new-role-dialog")
    dialog.locator('input[name="code"]').fill(role_name)
    dialog.locator('input[name="name"]').fill(role_name)
    dialog.locator('input[name="description"]').fill(f"Ve sólo {code}.")
    with page.expect_response(_response_for("/web/roles", "POST")):
        dialog.get_by_role("button", name=e2e_copy("create_role")).click()

    row = page.locator("#role-list-inner > div > div", has_text=role_name)
    row.locator(f'button[aria-label="{e2e_copy("edit_role")}"]').click()
    edit_dialog = page.locator("#role-edit-dialog")
    edit_dialog.locator("label", has_text=code).locator(
        'input[name="permission_ids"]'
    ).check()
    with page.expect_response(_response_for("/web/roles/matrix", "POST")):
        edit_dialog.get_by_role("button", name=e2e_copy("save_permissions")).click()


def _create_user_with_role(
    page: Page, *, base_url: str, username: str, password: str, role_name: str
) -> None:
    """A user holding exactly one role, created through the real screens."""
    page.goto(f"{base_url}/users")
    page.get_by_role("button", name=e2e_copy("new_user")).click()
    dialog = page.locator("#new-user-dialog")
    dialog.locator('input[name="username"]').fill(username)
    dialog.locator('input[name="display_name"]').fill(username)
    dialog.locator('input[name="password"]').fill(password)
    with page.expect_response(_response_for("/web/users", "POST")):
        dialog.get_by_role("button", name=e2e_copy("create_user")).click()
    expect(page.locator("#user-list")).to_contain_text(username)

    # Scoped to the CREATED user's row: the `sistema` sentinel row also has a
    # roles button, so the click must name whose row it means.
    page.locator("#user-list-inner > div > div", has_text=username).locator(
        f'button[aria-label="{e2e_copy("assign_roles")}"]'
    ).click()
    roles_dialog = page.locator("#user-edit-dialog")
    expect(roles_dialog).to_contain_text(f"Roles — {username}")
    roles_dialog.locator("label", has_text=role_name).locator(
        'input[name="role_ids"]'
    ).check()
    with page.expect_response(_response_for("/web/users/roles", "POST")):
        roles_dialog.get_by_role("button", name=e2e_copy("save_roles")).click()


def _log_in_through_the_form(page: Page, base_url: str, username: str, password: str):
    """Drive the real login form and return the page the browser lands on."""
    page.goto(f"{base_url}/login")
    page.get_by_label(e2e_copy("username")).fill(username)
    page.get_by_label(e2e_copy("password")).fill(password)
    page.get_by_role("button", name=e2e_copy("sign_in")).click()


def _change_confined_password(page: Page, *, current: str, new: str) -> None:
    """Complete the confined password change through the real form."""
    page.get_by_label(e2e_copy("current_password")).fill(current)
    page.get_by_label(e2e_copy("new_password"), exact=True).fill(new)
    page.get_by_label(e2e_copy("confirm_password")).fill(new)
    page.get_by_role("button", name=e2e_copy("save_password")).click()


# ---------------------------------------------------------------------------
# The drawer's facts
# ---------------------------------------------------------------------------


def test_the_drawer_opens_with_the_document_facts(page: Page, api: ApiClient) -> None:
    """Clicking a row's identifier opens the drawer with that document's facts.

    The claims are rendered content, not wiring: the sale drawer carries the
    customer's name and the confirmed sale's assigned number and total, the
    purchase drawer the supplier's name and its total, and both bodies carry
    the detail partial's `#document-detail-inner` marker — the fragment really
    swapped into this page's drawer, not a link that navigated away.
    """
    account_id = create_account_with_methods(api, "Caja", ("Cash",))
    product_id = create_product(
        api,
        sku="DOCS-WIDGET",
        name="Docs Widget",
        sale_price="10.00",
        cost_price="5.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )["id"]
    customer_id = create_customer(api, "Drawer Buyer")
    supplier_id = create_supplier(api, "Drawer Supplier")

    sale_id = create_confirmed_credit_sale(
        api, customer_id, product_id, qty="1", unit_price="10.00"
    )
    sale_number = api.get_json(f"/api/sales/{sale_id}")["sale"]["sale_number"]
    purchase_id = create_confirmed_credit_purchase(
        api, supplier_id, product_id, qty="3", unit_cost="10.00"
    )

    _open_documents_page(page, api)

    # The sale row's identifier opens the drawer with the sale's facts.
    _open_drawer(page, "sale", sale_id)
    body = page.locator(_DETAIL_INNER)
    expect(body).to_contain_text("Drawer Buyer")
    expect(body).to_contain_text(str(sale_number))
    expect(body).to_contain_text("10.00")

    # The purchase row's identifier swaps the same drawer to the purchase.
    _open_drawer(page, "purchase", purchase_id)
    expect(body).to_contain_text("Drawer Supplier")
    expect(body).to_contain_text("30.00")


def test_escape_closes_the_drawer_and_empties_its_body(page: Page, api: ApiClient) -> None:
    """Escape hides the drawer AND empties the body's DOM.

    The emptied body is the property no Rust test can reach: it is what the
    page's close script does to the live DOM after the keypress, so the next
    open can never show a previous document's facts for an instant.
    """
    product_id = create_product(
        api, sku="ESCAPE-WIDGET", name="Escape Widget", stock="5",
        min_stock="1", max_stock="100",
    )["id"]
    customer_id = create_customer(api, "Escape Buyer")
    sale_id = create_confirmed_credit_sale(api, customer_id, product_id)

    _open_documents_page(page, api)
    _open_drawer(page, "sale", sale_id)
    expect(page.locator(_DETAIL_INNER)).to_contain_text("Escape Buyer")

    page.keyboard.press("Escape")

    expect(page.locator(_DRAWER)).not_to_be_visible()
    assert page.evaluate(
        "document.getElementById('document-drawer-body').innerHTML"
    ) == "", "closing the drawer must empty its body, not just hide it"


def test_opening_the_drawer_never_navigates_or_reloads(page: Page, api: ApiClient) -> None:
    """Opening the drawer leaves the URL and the page itself untouched.

    The trigger carries no ``hx-push-url``, so the claim is the browser's own
    state: the URL still reads `/documents`, and a marker set in the page's
    JavaScript before the click survives the open — a navigation or reload
    would create a fresh world and lose it.
    """
    product_id = create_product(
        api, sku="URL-WIDGET", name="Url Widget", stock="5",
        min_stock="1", max_stock="100",
    )["id"]
    customer_id = create_customer(api, "Url Buyer")
    sale_id = create_confirmed_credit_sale(api, customer_id, product_id)

    _open_documents_page(page, api)
    page.evaluate("window.__drawer_probe = 'alive'")
    before = page.url

    _open_drawer(page, "sale", sale_id)

    expect(page.locator(_DRAWER)).to_be_visible()
    assert page.url == before, f"the drawer open moved the URL: {before} -> {page.url}"
    assert page.evaluate("window.__drawer_probe") == "alive", (
        "the page reloaded while opening the drawer"
    )


def test_the_impact_preview_renders_before_the_button(page: Page, api: ApiClient) -> None:
    """The preview stands above the button, per state, before it is pressed.

    A DRAFT's delete preview says what a draft delete removes and what it
    never did; a CONFIRMED sale's annulment preview lists the effects cancel
    will create — the stock-return movement for the tracked line and the
    refund for its payment. The operator must be able to read these before
    any press; the test reads them the way the operator does, off the page.
    """
    account_id = create_account_with_methods(api, "Caja", ("Cash",))
    method_id = account_method_id(api, account_id, "Cash")
    product_id = create_product(
        api,
        sku="IMPACT-WIDGET",
        name="Impact Widget",
        sale_price="10.00",
        cost_price="5.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )["id"]
    customer_id = create_customer(api, "Impact Buyer")

    draft_id = create_sale_draft(api, customer_id)
    add_sale_line(api, draft_id, product_id, qty="1", unit_price="10.00")

    confirmed_id = create_sale_draft(api, customer_id)
    add_sale_line(api, confirmed_id, product_id, qty="1", unit_price="10.00")
    api.post_json(f"/api/sales/{confirmed_id}/confirm", {"method_id": method_id})

    _open_documents_page(page, api)

    # The draft: the delete action with its impact above the button.
    _open_drawer(page, "sale", draft_id)
    body = page.locator(_DETAIL_INNER)
    expect(body).to_contain_text(e2e_copy("delete_draft_impact"))
    expect(body).to_contain_text(e2e_copy("never_confirmed"))
    delete_button = body.locator('button[hx-delete="/web/sales/%d"]' % draft_id)
    expect(delete_button).to_be_visible()

    # The confirmed sale: the annulment's effects, listed before its button.
    _open_drawer(page, "sale", confirmed_id)
    expect(body).to_contain_text(e2e_copy("sale_return_stock"))
    expect(body).to_contain_text("Impact Widget")
    expect(body).to_contain_text(e2e_copy("refund_account"))
    expect(body).to_contain_text("Caja")
    expect(body.locator('form[hx-post="/web/sales/cancel"]')).to_be_visible()


def test_deleting_a_draft_refreshes_the_feed_and_closes_the_drawer(
    page: Page, api: ApiClient
) -> None:
    """The confirmed delete re-reads the feed, closes the drawer, stays put.

    The row disappears because the page re-read `#document-list` (the
    `sale-changed` event's refresh), the drawer hides and its body empties,
    and the URL never moves — the whole reaction to one DELETE, as the
    browser lives it.
    """
    product_id = create_product(
        api, sku="DELETE-WIDGET", name="Delete Widget", stock="5",
        min_stock="1", max_stock="100",
    )["id"]
    customer_id = create_customer(api, "Delete Buyer")
    sale_id = create_sale_draft(api, customer_id)
    add_sale_line(api, sale_id, product_id, qty="1", unit_price="10.00")

    _open_documents_page(page, api)
    listing = page.locator(_DOCUMENT_LIST)
    expect(listing).to_contain_text(f"{e2e_copy('draft_ref')}{sale_id}")

    _open_drawer(page, "sale", sale_id)
    seen = _answer_next_dialog(page, accept=True)
    with page.expect_response(_response_for(f"/web/sales/{sale_id}", "DELETE")):
        page.locator(_DETAIL_INNER).locator(
            f'button[hx-delete="/web/sales/{sale_id}"]'
        ).click()
    assert e2e_copy("no_undo") in seen[0], seen

    # The feed re-read lands the row's disappearance; expect() polls for it
    # instead of sleeping.
    expect(listing).not_to_contain_text(f"Draft #{sale_id}")
    expect(page.locator(_DRAWER)).not_to_be_visible()
    assert page.evaluate(
        "document.getElementById('document-drawer-body').innerHTML"
    ) == "", "the closed drawer must be empty, not merely hidden"
    assert urlparse(page.url).path == _DOCUMENTS_PAGE, (
        f"the delete navigated away: {page.url}"
    )


def test_annulling_a_confirmed_sale_refreshes_the_feed_and_closes_the_drawer(
    page: Page, api: ApiClient
) -> None:
    """The drawer's annul form presses like the delete: dialog, feed, close.

    The POST carries the hidden `sale_id` the form renders — the flow below
    only works when that field carries the document's id — and the
    `sale-changed` reaction is the same one the delete proved: the feed
    re-reads (the row's status line becomes the annulled state), the drawer
    hides and empties, and the URL never moves.
    """
    product_id = create_product(
        api, sku="ANNUL-WIDGET", name="Annul Widget", stock="5",
        min_stock="1", max_stock="100",
    )["id"]
    customer_id = create_customer(api, "Annul Buyer")
    sale_id = create_confirmed_credit_sale(api, customer_id, product_id)
    sale_number = api.get_json(f"/api/sales/{sale_id}")["sale"]["sale_number"]

    _open_documents_page(page, api)
    listing = page.locator(_DOCUMENT_LIST)
    expect(listing).to_contain_text(sale_number)

    _open_drawer(page, "sale", sale_id)
    body = page.locator(_DETAIL_INNER)
    form = body.locator('form[hx-post="/web/sales/cancel"]')
    expect(form).to_be_visible()
    expect(form.locator('input[name="sale_id"]')).to_have_value(str(sale_id))

    seen = _answer_next_dialog(page, accept=True)
    with page.expect_response(_response_for("/web/sales/cancel", "POST")):
        form.locator('button[type="submit"]').click()
    assert e2e_copy("no_undo") in seen[0], seen

    # The feed re-read lands the row's new state; expect() polls for it.
    expect(
        listing.locator('[data-document-kind="sale"]', has_text=sale_number)
    ).to_contain_text(e2e_copy("cancelled"))
    expect(page.locator(_DRAWER)).not_to_be_visible()
    assert page.evaluate(
        "document.getElementById('document-drawer-body').innerHTML"
    ) == "", "the closed drawer must be empty, not merely hidden"
    assert urlparse(page.url).path == _DOCUMENTS_PAGE, (
        f"the annul navigated away: {page.url}"
    )


def test_a_refused_annul_names_the_action_and_the_server_message(
    page: Page, api: ApiClient
) -> None:
    """A server refusal reads like the sibling forms', never as a raw path.

    The drawer's annul form carries ``data-action`` like every sibling form,
    so the page's global error handler names the action the operator pressed
    and appends the server's message verbatim. The refusal is real: a cash
    sale whose paying account was drained, so the refund the annulment would
    post is refused for the negative balance the endpoint enforces.
    """
    account_id = create_account_with_methods(api, "Caja", ("Cash",))
    method_id = account_method_id(api, account_id, "Cash")
    product_id = create_product(
        api, sku="REFUSE-WIDGET", name="Refuse Widget", stock="5",
        min_stock="1", max_stock="100",
    )["id"]
    customer_id = create_customer(api, "Refuse Buyer")
    sale_id = create_sale_draft(api, customer_id)
    add_sale_line(api, sale_id, product_id, qty="1", unit_price="10.00")
    # The cash confirm pays 10.00 into the account...
    api.post_json(f"/api/sales/{sale_id}/confirm", {"method_id": method_id})
    # ...and the drain leaves nothing for the annulment's refund to pay from.
    api.post_json(
        "/api/transactions",
        {
            "account_id": account_id,
            "type": "Expense",
            "amount": "10.00",
            "description": None,
            "reference": None,
            "date": "2024-05-03",
        },
    )

    _open_documents_page(page, api)
    _open_drawer(page, "sale", sale_id)
    body = page.locator(_DETAIL_INNER)
    form = body.locator('form[hx-post="/web/sales/cancel"]')
    expect(form).to_be_visible()

    _answer_next_dialog(page, accept=True)
    with page.expect_response(_response_for("/web/sales/cancel", "POST")):
        form.locator('button[type="submit"]').click()

    # The refusal names the pressed action, then the server's own message.
    notice = page.locator("[data-notice='error']")
    expect(notice).to_contain_text(e2e_copy("annul_failed"))
    expect(notice).to_contain_text("negative balance")
    # The refusal leaves the operator with the document, not a closed drawer.
    expect(page.locator(_DRAWER)).to_be_visible()


def test_a_sales_read_only_principal_sees_only_the_sales_family(
    page: Page, api: ApiClient
) -> None:
    """The narrowing holds in the rendered page, rows included.

    A principal holding exactly `sales.read` opens `/documents` and the feed
    shows only its family: every row is a sale, the type filter offers only
    the sales group, no other family's row exists to click, and the one sale
    row's drawer really opens — the narrowing is a rendered, clickable feed,
    not an attribute.
    """
    product_id = create_product(
        api, sku="NARROW-WIDGET", name="Narrow Widget", stock="5",
        min_stock="1", max_stock="100",
    )["id"]
    customer_id = create_customer(api, "Narrow Buyer")
    supplier_id = create_supplier(api, "Narrow Supplier")
    sale_id = create_sale_draft(api, customer_id)
    add_sale_line(api, sale_id, product_id, qty="1", unit_price="10.00")
    purchase_id = create_confirmed_credit_purchase(
        api, supplier_id, product_id, qty="1", unit_cost="10.00"
    )
    _ = purchase_id

    # The narrowed principal, built through the real screens: a role whose
    # matrix holds exactly sales.read, a user holding it, and the confined
    # password change that lets the session reach the app.
    _create_role_with_one_permission(page, base_url=api.base_url, code="sales.read", role_name="solo_ventas")
    _create_user_with_role(page, base_url=api.base_url, username="ventas1", password=INITIAL_PASSWORD, role_name="solo_ventas")

    # The identity tests' anonymous-context pattern: this test owns the
    # session it logs in, so the harness cookie must not leak into it.
    context = page.context.browser.new_context()
    visitor = context.new_page()
    try:
        _log_in_through_the_form(
            visitor, api.base_url, "ventas1", INITIAL_PASSWORD
        )
        expect(visitor).to_have_url(f"{api.base_url}/password")
        _change_confined_password(
            visitor, current=INITIAL_PASSWORD, new=CHANGED_PASSWORD
        )
        expect(visitor.locator("[data-notice='error']")).to_contain_text(
            "dashboard.read"
        )

        _open_documents_page(visitor, api)
        feed = visitor.locator(f"{_DOCUMENT_LIST} #document-list-inner")

        # The one readable family is rendered and its drawer really opens.
        expect(feed.locator(f'[data-document-kind="sale"]')).to_have_count(1)
        _open_drawer(visitor, "sale", sale_id)
        expect(visitor.locator(_DETAIL_INNER)).to_contain_text("Narrow Buyer")
        visitor.keyboard.press("Escape")

        # No other family rendered a row, and no other family's detail is a
        # click away: the only detail buttons the feed carries are sale ones
        # (there is no sale payment in this seed, so the count is exact), and
        # the purchase the seed created surfaces nowhere.
        for kind in ("purchase", "purchase_payment", "receipt", "stock_movement"):
            expect(feed.locator(f'[data-document-kind="{kind}"]')).to_have_count(0)
        all_buttons = feed.locator('button[hx-get^="/web/documents/detail/"]')
        sale_buttons = feed.locator(
            'button[hx-get^="/web/documents/detail/sale/"]'
        )
        expect(all_buttons).to_have_count(1)
        expect(sale_buttons).to_have_count(1)

        # The type filter offers only the groups whose families this
        # principal may read. `sales.read` owns the Sale AND SalePayment
        # families (src/models.rs `read_code`), so the options are exactly
        # "Ventas" and "Pagos" — the purchase and stock groups never render.
        options = visitor.locator(
            "main form#document-filters option[data-document-group]"
        )
        expect(options).to_have_count(2)
        expect(options.first).to_have_attribute("data-document-group", "sales")
        expect(options.last).to_have_attribute("data-document-group", "payments")
        assert visitor.locator('[data-document-group="purchases"]').count() == 0
        assert visitor.locator('[data-document-group="stock"]').count() == 0
    finally:
        context.close()
