"""Slice T10 (confirmed-only cost freshness): the warning lives at confirm time.

A purchase line's cost is provisional while the purchase is a draft: the line
can still be edited or deleted and the purchase may never be confirmed, so a
rising-cost comparison there would assert something the domain does not yet
know. Confirming is when the cost becomes a fact, so the stale-cost warning —
both numbers and the "Apply to product" button — renders only on a Confirmed
purchase, and the apply route refuses a draft. A confirmed table renders four
header cells (the remove column is draft-only), hence the warning sub-row's
colspan of 4.

The Rust suite already covers the pieces (the confirmed-only gate in
`record_from_detail`, the route's draft refusal, the recomputation); what only
a browser can prove is the moment an operator lives: the warning is not there
on the draft, the operator clicks Confirm, and the warning arrives in the
swapped fragment — no reload — with the button ready. Clicking it must write
the line's cost into the product, and the stored-state assertion reads the
product through the API, so the test fails if the button posts and the write
silently stops firing.

The recomputed price is asserted against the value the markup derivation
produces in Python (cost × (1 + markup/100), pinned to cents), not against
whatever the product happens to return: if the derivation ever silently
stopped and the price kept its old value, the test would fail on the number,
not on a shrug.
"""

from __future__ import annotations

import time
from datetime import date, timedelta
from decimal import ROUND_HALF_UP, Decimal
from urllib.parse import urlparse

from playwright.sync_api import Page, expect

from helpers import (
    ApiClient,
    account_method_id,
    add_purchase_line,
    add_sale_line,
    confirm_purchase,
    create_account_with_methods,
    create_customer,
    create_product,
    create_purchase_draft,
    create_sale_draft,
    create_supplier,
    e2e_copy,
    fund_account,
)


def _response_for(path: str, method: str = "GET"):
    """Predicate matching one request path and method, query string ignored."""

    def matches(response) -> bool:
        return urlparse(response.url).path == path and response.request.method == method

    return matches


def _derived_sale_price(cost: str, markup_pct: str) -> Decimal:
    """The sale price the markup derivation must produce: cost × (1 + m/100).

    Pinned to cents with half-up rounding, the same retail convention the
    Rust helper implements (`round_derived_price_to_cents`). Computing the
    expectation in the test keeps the derivation provable: a product whose
    sale price stops being recomputed cannot satisfy this number.
    """
    cost_dec = Decimal(cost)
    factor = Decimal("1") + Decimal(markup_pct) / Decimal("100")
    return (cost_dec * factor).quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)


def _rising_cost_purchase(
    api: ApiClient, *, sku: str, name: str, supplier_name: str
) -> tuple[int, int, int, int]:
    """A credit draft whose line cost (12.00) rises over the product cost (10.00).

    Credit is the payment type a draft confirms without a payment method —
    the service derives no cash account and posts no Expense — so the confirm
    in the browser needs no method selection and the test stays about the
    warning, not about sourcing a Cash account. Returns the ids the tests
    need: product, purchase, line, and the derived-price inputs live in the
    callers.
    """
    product = create_product(
        api,
        sku=sku,
        name=name,
        # A manual price that no derivation could produce by accident: if the
        # seed's own derived price were not 15.00, the later recomputation
        # would be indistinguishable from the manual price sticking.
        sale_price="99.00",
        cost_price="10.00",
        markup_pct="50",
        stock="5",
        min_stock="1",
        max_stock="50",
    )
    product_id = int(product["id"])
    supplier_id = create_supplier(api, supplier_name)
    purchase_id = create_purchase_draft(
        api, supplier_id, payment_type="Credit", due_date="2024-06-01"
    )
    add_purchase_line(api, purchase_id, product_id, qty="2", unit_cost="12.00")
    line_id = int(
        next(
            line["id"]
            for line in api.get_json(f"/api/purchases/{purchase_id}")["lines"]
            if int(line["product_id"]) == product_id
        )
    )
    return product_id, purchase_id, line_id, supplier_id


def _assert_no_warning(page: Page) -> None:
    """Assert the page carries neither the warning nor its action.

    `stale cost` appears in exactly two templates: this warning's badge and
    the product drawer's badge (templates/partials/product_detail.html), which
    never renders on a purchase page — so a page-level absence is
    discriminating. The button is the action the warning carries, so its
    absence follows.
    """
    expect(page.get_by_text("stale cost", exact=True)).to_have_count(0)
    expect(page.get_by_role("button", name="Apply to product")).to_have_count(0)


def _assert_peek_body_is_the_empty_state(page: Page) -> None:
    """Assert the closed peek's body holds the shared empty state and no detail.

    The claim is NOT "the peek is hidden": `not_to_be_visible()` already says
    that, and it cannot tell an emptied body from a full one. It is "the purchase
    is gone and the panel says so", because at and above the drawer's split
    threshold the panel is a permanent column and a body with nothing in it is
    what the operator stares at. The child count is what keeps this honest — a
    leftover fragment carrying the same words would pass on text alone.
    """
    assert page.evaluate(
        "document.querySelectorAll('#purchase-drawer-body > .empty').length"
    ) == 1, "the closed peek must be back to the empty state, not merely hidden"
    assert page.evaluate(
        "document.getElementById('purchase-drawer-body').textContent.trim()"
    ) == e2e_copy("drawer_empty"), "the closed body must hold no purchase, only the empty state"


def test_confirming_a_rising_cost_purchase_warns_and_applying_updates_the_product(
    page: Page, api: ApiClient
) -> None:
    """The warning arrives with the confirm swap, and Apply really moves the product.

    The moment the maintainer described: on the draft there is no warning (a
    draft line's cost is provisional), the operator confirms, and the confirm
    response — which swaps `#purchase-record` itself — brings the warning in
    without a reload. The failure this catches beyond the Rust route tests:
    the gate flipped to always-on (warning on the draft too) or the confirm
    fragment losing the money table, which would leave the operator confirming
    blind. The no-reload claim is proven with a JS marker: a full navigation
    would wipe it.

    Then the journey's payoff — clicking Apply to product — and the stored
    state is read through the API: the cost moved AND the sale price was
    recomputed by the markup, not left stale behind the new cost. Asserting
    the recomputed price against the derived value (12.00 × 1.5) fails the
    test the moment the derivation stops firing.
    """
    markup = "50"
    product_id, purchase_id, line_id, _supplier_id = _rising_cost_purchase(
        api, sku="APPLY-SKU-01", name="Apply Widget", supplier_name="Apply Supplier"
    )

    # The markup must be ACTIVE at seed time, or the recomputation the click
    # triggers proves nothing: assert the stored price is already the derived
    # one before any click happens.
    starting = api.get_json(f"/api/products/{product_id}")
    assert Decimal(str(starting["sale_price"])) == _derived_sale_price("10.00", markup), (
        starting
    )
    starting_price = Decimal(str(starting["sale_price"]))

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    # A draft asserts nothing: the cost is provisional, so no warning, no
    # button — the state the warning must be born out of.
    _assert_no_warning(page)
    # A full navigation clears window properties; if the marker survives to
    # the last assertion, everything after this line happened without one.
    page.evaluate("window.__t10_no_reload = true")

    # Confirm from the browser: the action lives behind the bar's Confirm ▾
    # (which opens the dialog), and a Credit confirm carries no method control
    # at all (the dialog shows the due summary instead), so the submit click
    # is the whole confirm.
    with page.expect_response(
        _response_for(f"/web/purchases/{purchase_id}/confirm", "POST")
    ):
        page.locator("#open-confirm").click()
        page.locator("#confirm-purchase button[type='submit']").click()

    expect(page.locator("#purchase-record")).to_contain_text("Confirmed")
    # Both numbers in the warning's own sentence, as one exact text node: a
    # line row repeating an amount cannot satisfy it, and either half dropping
    # silently breaks the match.
    warning = page.get_by_text("line cost 12.00 USD • stored 10.00 USD", exact=True)
    expect(warning).to_be_visible()
    expect(page.get_by_text("stale cost", exact=True)).to_have_count(1)
    apply_button = page.get_by_role("button", name="Apply to product")
    expect(apply_button).to_be_visible()
    # The warning appeared in the confirm's swapped fragment, not in a reload.
    assert page.evaluate("window.__t10_no_reload") is True

    with page.expect_response(
        _response_for(f"/web/purchases/{purchase_id}/lines/{line_id}/apply-cost", "POST")
    ):
        apply_button.click()

    # The route swaps `#purchase-record` with the refreshed record: the warning
    # is gone while the line itself stays, so the swap really happened and no
    # full reload was needed.
    expect(page.get_by_text("stale cost", exact=True)).to_have_count(0)
    row = page.locator("#purchase-record-money table tbody tr", has_text="Apply Widget")
    expect(row).to_have_count(1)

    # Stored state, read back through the API: the cost moved AND the sale
    # price was recomputed by the markup, not left stale behind the new cost.
    stored = api.get_json(f"/api/products/{product_id}")
    assert Decimal(str(stored["cost_price"])) == Decimal("12.00"), stored
    expected_price = _derived_sale_price("12.00", markup)
    assert expected_price != starting_price, (
        "the test is not discriminating: the derived price equals the starting one"
    )
    assert Decimal(str(stored["sale_price"])) == expected_price, stored
    assert Decimal(str(stored["markup_pct"])) == Decimal(markup), stored


def test_a_draft_purchase_with_a_rising_line_cost_shows_no_warning_and_no_apply(
    page: Page, api: ApiClient
) -> None:
    """A draft stays silent even when the line cost rises: that is the correction.

    This is the behaviour that was wrong: the old build warned on a draft,
    where the cost is provisional — the line can be edited or deleted and the
    purchase may never be confirmed, so the comparison asserted something the
    domain did not yet know. The failure this pins is the Confirmed gate
    falling out of `record_from_detail` again: the UI would warn on a cost
    that is not yet a fact, and offer an Apply action the route rightly
    refuses for a draft.
    """
    product_id, purchase_id, _line_id, _supplier_id = _rising_cost_purchase(
        api, sku="DRAFT-SILENT-SKU", name="Draft Silent Widget", supplier_name="Draft Silent Supplier"
    )
    assert product_id  # the ids shape the seed; the page is what is asserted

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    # The line's cost (12.00) is higher than the stored cost (10.00): the same
    # comparison that fires on a confirmed purchase must stay silent here.
    _assert_no_warning(page)


def test_a_confirmed_purchase_with_equal_costs_shows_no_warning(
    page: Page, api: ApiClient
) -> None:
    """Equal costs are the fresh state, confirmed too: no warning, no button.

    The failure this pins is the disagreement gate collapsing to always-true
    once the Confirmed gate is in place: the UI would then offer the action on
    every confirmed line even when there is nothing to apply, and a click
    would rewrite the product with its own stored cost while claiming
    freshness work happened. Confirmed via the API first — the journey test
    already owns the browser confirm; this test is about the state after it.
    """
    product = create_product(
        api,
        sku="FRESH-LINE-SKU",
        name="Fresh Line Widget",
        cost_price="8.00",
        stock="4",
        min_stock="1",
        max_stock="40",
    )
    product_id = int(product["id"])
    supplier_id = create_supplier(api, "Fresh Line Supplier")
    purchase_id = create_purchase_draft(
        api, supplier_id, payment_type="Credit", due_date="2024-06-01"
    )
    add_purchase_line(api, purchase_id, product_id, qty="1", unit_cost="8.00")
    # Credit confirms with no payment method; the read-back in the helper
    # fails the seed if the confirm silently no-oped.
    confirm_purchase(api, purchase_id)

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    _assert_no_warning(page)


# ---------------------------------------------------------------------------
# S6 follow-ups: the rewritten row must survive a 360 px phone (no horizontal
# scroll, even with a supplier name long enough to need truncation), and the
# peek contract S3 shipped must actually open from /purchases — the drawer
# tests only ever drove /documents, which is how a red suite shipped once.
# ---------------------------------------------------------------------------


# Deliberately longer than a 360 px viewport can show without truncating:
# the row's supplier cell carries `truncate`, and this name proves truncation
# is exercised rather than assumed (a wide unbreakable string would widen the
# row and scroll the page if the grid ever lost its min-w-0).
_LONG_SUPPLIER = "Distribuidora Mayorista de Almacen y Alimentos del Litoral SRL"


def _seed_purchase_states(api: ApiClient) -> dict[str, int]:
    """A draft, an owed confirmed purchase and a settled one, for one supplier.

    The owed purchase is a confirmed Credit with a due date 30 days out
    (the Due chip, something owed, not yet past); the settled one is a
    confirmed Cash purchase through the seeded account — the confirm itself
    posts the payment, so due lands at 0 in one call. Returns the ids the
    tests assert against, including the owed purchase's due, read back
    through the API so the chip's residual amount is never guessed.
    """
    product = create_product(
        api,
        sku="PEEK-S6-SKU",
        name="Peek S6 Widget",
        sale_price="20.00",
        cost_price="5.00",
        stock="30",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    supplier_id = create_supplier(api, _LONG_SUPPLIER)
    # A due date relative to the day the test runs: fixed 2024 dates are
    # already past, and the row would honestly render Overdue instead of Due.
    future_due = (date.today() + timedelta(days=30)).isoformat()

    account_id = create_account_with_methods(api, "Peek Caja", ("Cash",))
    cash = account_method_id(api, account_id, "Cash")
    fund_account(api, account_id, "1000")

    draft_id = create_purchase_draft(
        api, supplier_id, payment_type="Credit", due_date=future_due
    )
    add_purchase_line(api, draft_id, product_id, qty="1", unit_cost="6.00")

    settled_id = create_purchase_draft(api, supplier_id, payment_type="Cash")
    add_purchase_line(api, settled_id, product_id, qty="3", unit_cost="6.00")
    # A Cash confirm pays the total at once (one payment posted), so this row
    # settles to due = 0 without a second call.
    confirm_purchase(api, settled_id, method_id=cash)
    due = str(api.get_json(f"/api/purchases/{settled_id}")["due"])
    assert Decimal(due) == 0, "the settled purchase must carry due = 0"

    owed_id = create_purchase_draft(
        api, supplier_id, payment_type="Credit", due_date=future_due
    )
    add_purchase_line(api, owed_id, product_id, qty="2", unit_cost="6.00")
    confirm_purchase(api, owed_id)
    owed_due = str(api.get_json(f"/api/purchases/{owed_id}")["due"])
    assert Decimal(owed_due) > 0, "the owed purchase must carry due > 0"
    return {
        "draft": draft_id,
        "owed": owed_id,
        "settled": settled_id,
        "owed_due": owed_due,
    }


def test_purchase_list_at_360px_never_scrolls_horizontally(page: Page, api: ApiClient):
    """AC2, proven in a browser: no horizontal scroll at 360 px, on both lists.

    The Rust suite can read classes, but only a real layout can prove the
    rewrite's responsive claim: the row's grid must keep every column inside
    a 360 px viewport even when the supplier name is far longer than the
    viewport — the long name makes truncation load-bearing, so a lost
    `min-w-0` or `truncate` would widen the page and fail the assertion.
    Draft, owed and settled rows are all present, so every chip shape is
    laid out, and /sales is checked with its own rows as the sibling list
    the same viewport must hold.

    The suite sets no viewport anywhere (no precedent in e2e/), so this test
    uses Playwright's own idiom, `page.set_viewport_size`, on the shared
    fixture's page. The scroll claim is polled as a resolved browser
    condition via `page.wait_for_function`, never a sleep.
    """
    seeded = _seed_purchase_states(api)

    # A sale row too: /sales must hold the same viewport, and a customer with
    # a long name stresses its flex row the way the supplier stresses ours.
    long_buyer = "Compradora con Nombre Notablemente Extenso para Moviles"
    customer_id = create_customer(api, long_buyer)
    product_id = int(create_product(
        api,
        sku="SALES360-SKU",
        name="Sales 360 Widget",
        sale_price="10.00",
        cost_price="4.00",
        stock="10",
        min_stock="1",
        max_stock="50",
    )["id"])
    sale_id = create_sale_draft(api, customer_id, payment_type="Credit", due_date="2024-06-01")
    add_sale_line(api, sale_id, product_id, qty="1", unit_price="10.00")

    page.set_viewport_size({"width": 360, "height": 740})

    # The AC2 claim itself: the page must not be wider than its viewport.
    # `wait_for_function` polls the resolved condition in the browser (no
    # sleep): it returns as soon as the document fits, and times out — with
    # the scrollWidth that broke it — when the layout overflows.
    no_horizontal_scroll = (
        "document.documentElement.scrollWidth <= "
        "document.documentElement.clientWidth"
    )

    # /purchases first, all three seeded rows rendered before the claim is
    # read: the wait is on the rows being visible, so the layout is final.
    with page.expect_response(_response_for("/web/purchases")):
        page.goto(f"{api.base_url}/purchases")
    for kind in ("draft", "owed", "settled"):
        expect(page.locator(f"#purchase-{seeded[kind]}")).to_be_visible()
    # The long supplier really is on the page being measured: truncation is
    # exercised, not assumed.
    expect(page.locator("#purchase-list-inner")).to_contain_text(_LONG_SUPPLIER)
    page.wait_for_function(no_horizontal_scroll)

    # The owed row's chip carries the residual amount, at this width too.
    owed_row = page.locator(f"#purchase-{seeded['owed']}")
    expect(owed_row).to_contain_text(f"Due {seeded['owed_due']}")

    # /sales under the same viewport, its own long name in the row.
    with page.expect_response(_response_for("/web/sales")):
        page.goto(f"{api.base_url}/sales")
    expect(page.locator(f"#sale-{sale_id}")).to_be_visible()
    expect(page.locator("#sale-list-inner")).to_contain_text(long_buyer)
    page.wait_for_function(no_horizontal_scroll)


def test_clicking_a_purchase_row_opens_the_peek_and_escape_closes_it(
    page: Page, api: ApiClient
):
    """On /purchases, a row click opens the read-only peek; Escape empties it.

    S3 made the purchase row open the peek and S6 rewrote the row, but every
    drawer test drove /documents — the gap that let a red browser suite ship
    once. This proves the whole journey on the page operators live on: the
    row's `hx-get` swaps the document detail into `#purchase-drawer-body`,
    the page's `htmx:afterSwap` listener reveals `#purchase-drawer`, the
    body carries THIS document's facts (its identifier — number or draft
    handle — and the supplier's name), and Escape both hides the drawer and
    empties the body so a next open can never flash the previous purchase.

    The row is a confirmed credit purchase, so its drawer title is the
    assigned `2024-PURCH-NNNNNN` number the API read back — a stable handle
    the body must carry exactly once. Every wait is an `expect()` condition
    or a `page.expect_response`; no sleeps.
    """
    product = create_product(
        api,
        sku="OPEN-S6-SKU",
        name="Open S6 Widget",
        sale_price="15.00",
        cost_price="7.00",
        stock="12",
        min_stock="1",
        max_stock="60",
    )
    product_id = int(product["id"])
    supplier_name = "Peek Supplier"
    supplier_id = create_supplier(api, supplier_name)
    purchase_id = create_purchase_draft(
        api, supplier_id, payment_type="Credit", due_date="2024-06-01"
    )
    add_purchase_line(api, purchase_id, product_id, qty="2", unit_cost="7.00")
    confirm_purchase(api, purchase_id)
    purchase_number = str(
        api.get_json(f"/api/purchases/{purchase_id}")["purchase"]["purchase_number"]
    )

    with page.expect_response(_response_for("/web/purchases")):
        page.goto(f"{api.base_url}/purchases")
    row = page.locator(f"#purchase-{purchase_id}")
    expect(row).to_be_visible()

    drawer = page.locator("#purchase-drawer")
    drawer_body = page.locator("#purchase-drawer-body")
    expect(drawer).not_to_be_visible()

    # The click fires the row's hx-get; wait out its response, then the swap
    # listener reveals the drawer. The row's href would navigate, so this is
    # also the moment htmx's preventDefault is proven for real.
    with page.expect_response(
        _response_for(f"/web/documents/detail/purchase/{purchase_id}")
    ):
        row.click()
    expect(drawer).to_be_visible()
    expect(drawer_body).to_contain_text(purchase_number)
    expect(drawer_body).to_contain_text(supplier_name)
    # The URL never moved: the peek is an in-page swap, not a navigation.
    assert urlparse(page.url).path == "/purchases", page.url

    # Escape closes the drawer AND clears the body back to the empty state — the
    # body's DOM is the property the page's close script owns, the same claim the
    # documents drawer tests make for their sibling. "Cleared" means the shared
    # empty state, not nothing: on a wide screen the panel is a permanent column.
    page.keyboard.press("Escape")
    expect(drawer).not_to_be_visible()
    _assert_peek_body_is_the_empty_state(page)


# ---------------------------------------------------------------------------
# Rendered-colour gate: computed styles, not class lists.
#
# Every colour claim a class assertion makes can pass while the page still
# reads wrong — an earlier slice shipped a list row that read blue (the
# stylesheet colours anchors) while every class check passed, because the
# check looked at a span's classes instead of the element's rendered colour.
# getComputedStyle on the real element sees that class of defect, so the
# tests below read the browser's resolved colours against the design tokens:
#
#   --color-accent  #6ee7b7 → rgb(110, 231, 183)  (mint background)
#   base button label #0a0f0d → rgb(10, 15, 13)   (dark button label)
#   --color-bg      #0f1115 → rgb(15, 17, 21)     (page-action label, text-bg)
#   --color-text    #e6e8eb → rgb(230, 232, 235)  (normal text colour)
#   --color-accent2 #60a5fa → rgb(96, 165, 250)   (anchor blue — must NOT appear)
# ---------------------------------------------------------------------------

_MINT = "rgb(110, 231, 183)"
_BUTTON_LABEL = "rgb(10, 15, 13)"
_PAGE_ACTION_LABEL = "rgb(15, 17, 21)"
_TEXT = "rgb(230, 232, 235)"
_ANCHOR_BLUE = "rgb(96, 165, 250)"


def test_primary_actions_render_accent_colors_not_anchor_blue(
    page: Page, api: ApiClient
) -> None:
    """Buttons render mint with dark labels — proven by computed style, not class.

    The base `button` rule in the stylesheet sets `background-color:
    var(--color-accent)` and `color: #0a0f0d`, and the classes that once
    overrode it are gone, so the cascade is deterministic — but only a
    computed-style assertion can prove the cascade actually resolved that way
    in a real browser. This test checks both primary-button shapes operators
    touch: the shared page-header action (`[data-page-action]`) on /purchases,
    and a form submit button — the Confirm dialog's `<button type="submit">`
    on the purchase record page, the same button the journey test clicks to
    confirm a purchase.

    One resolved-value nuance the assertion pins deliberately: the page action
    carries `text-bg` in both component shapes — a `<button>` on /purchases
    (dialog mode, T3) and an `<a>` on pages whose action navigates — so its
    label resolves to `--color-bg` (#0f1115 → rgb(15, 17, 21)) — a different
    dark than the base button's #0a0f0d. Both are dark; neither is blue.

    The negative makes this a gate rather than a coincidence: no button on
    either page may compute to the old anchor blue, so any regression that
    re-colours a primary control fails loudly here.
    """
    supplier_id = create_supplier(api, "Colour Supplier")
    product = create_product(
        api,
        sku="COLOR-SKU-01",
        name="Colour Widget",
        sale_price="10.00",
        cost_price="5.00",
        stock="5",
        min_stock="1",
        max_stock="20",
    )
    purchase_id = create_purchase_draft(
        api, supplier_id, payment_type="Credit", due_date="2024-06-01"
    )
    add_purchase_line(
        api, purchase_id, int(product["id"]), qty="1", unit_cost="5.00"
    )

    # The page action on /purchases (the admin principal holds create
    # permission, so the "New purchase" action renders).
    with page.expect_response(_response_for("/web/purchases")):
        page.goto(f"{api.base_url}/purchases")
    action = page.locator("[data-page-action]")
    expect(action).to_be_visible()
    assert (
        action.evaluate("el => getComputedStyle(el).backgroundColor") == _MINT
    ), "the page action must render the mint accent background"
    assert action.evaluate("el => getComputedStyle(el).color") == _PAGE_ACTION_LABEL

    # The Confirm dialog's submit button on the record page: an unclassed
    # `<button type="submit">`, so the base button rule decides both colours.
    with page.expect_response(_response_for(f"/purchases/{purchase_id}")):
        page.goto(f"{api.base_url}/purchases/{purchase_id}")
    page.locator("#open-confirm").click()
    submit = page.locator("#confirm-purchase button[type='submit']")
    expect(submit).to_be_visible()
    assert (
        submit.evaluate("el => getComputedStyle(el).backgroundColor") == _MINT
    ), "the confirm submit button must render the mint accent background"
    assert submit.evaluate("el => getComputedStyle(el).color") == _BUTTON_LABEL

    # Negative gate: the old anchor blue must not be any button's background
    # on either page (transparent controls resolve to rgba(0, 0, 0, 0), which
    # also fails this check — as they should).
    for url in (
        f"{api.base_url}/purchases",
        f"{api.base_url}/purchases/{purchase_id}",
    ):
        page.goto(url)
        backgrounds = page.eval_on_selector_all(
            "button", "els => els.map(el => getComputedStyle(el).backgroundColor)"
        )
        assert _ANCHOR_BLUE not in backgrounds, (
            f"a button on {url} renders the old anchor blue: {backgrounds}"
        )


def test_purchase_list_row_renders_text_color_not_anchor_blue(
    page: Page, api: ApiClient
) -> None:
    """The row element itself renders in normal text colour, never anchor blue.

    This is the defect class the last red suite exposed: purchase rows are
    `<a>` elements, and the stylesheet colours anchors blue — so a row that
    lost its `text-text` utility reads blue while every class-level assertion
    still passes. The assertion here targets the ROW element
    (`#purchase-{id}`, the anchor itself) rather than a span inside it, which
    is exactly the check that would have caught the original defect: if the
    row went back to inheriting the anchor colour, `getComputedStyle` on the
    row would return rgb(96, 165, 250) and fail both assertions below.

    All three row shapes (draft, owed, settled) are seeded through the
    existing `_seed_purchase_states` helper and checked, so no chip state can
    hide a colour regression.
    """
    seeded = _seed_purchase_states(api)

    with page.expect_response(_response_for("/web/purchases")):
        page.goto(f"{api.base_url}/purchases")

    for kind in ("draft", "owed", "settled"):
        # The row anchor element itself — not a span inside it.
        row = page.locator(f"#purchase-{seeded[kind]}")
        expect(row).to_be_visible()
        colour = row.evaluate("el => getComputedStyle(el).color")
        assert colour == _TEXT, (
            f"the {kind} row must render in the normal text colour, got {colour}"
        )
        assert colour != _ANCHOR_BLUE, (
            f"the {kind} row renders the anchor blue — the exact defect this "
            "test exists to catch"
        )


def test_new_purchase_opens_the_dialog_prefilled_with_the_last_used_supplier(
    page: Page, api: ApiClient
) -> None:
    """The creation flow (purchases-create-and-header T3, AC3/AC4).

    `New purchase` opens `#new-purchase-dialog` — no `/purchases/new` page
    exists any more — and the dialog's supplier picker arrives pre-filled
    with the LAST USED supplier as a real, editable value, never a silent
    guess: the supplier is what resolves every line's default cost. Accepting
    the pre-filled default (Create draft) posts the existing
    `POST /web/purchases`, whose htmx branch answers `HX-Redirect`, so the
    browser lands on the new draft's record page — where the supplier must
    be the one the dialog offered. The stored record is read back through
    the API so a draft created for the wrong supplier cannot pass.
    """
    # A purchase that already exists, so the dialog has a last used supplier
    # to offer. Created through the API: the dialog flow itself is what the
    # browser drives below.
    supplier_name = "Dialog Default Supplier"
    supplier_id = create_supplier(api, supplier_name)
    create_purchase_draft(api, supplier_id, payment_type="Cash")

    with page.expect_response(_response_for("/web/purchases")):
        page.goto(f"{api.base_url}/purchases")

    action = page.locator("button[data-page-action]")
    expect(action).to_have_text("New purchase")
    action.click()
    dialog = page.locator("#new-purchase-dialog")
    expect(dialog).to_be_visible()

    picker_input = dialog.locator("#new-purchase-supplier")
    expect(picker_input).to_have_value(supplier_name)

    # Arm the response expectation around the click: the listener must exist
    # before the submit fires, or a fast response is missed and never seen.
    with page.expect_response(_response_for("/web/purchases", method="POST")):
        dialog.locator('form[data-action="Create purchase"]').get_by_role(
            "button", name="Create draft"
        ).click()
    page.wait_for_url("**/purchases/*")
    new_id = int(urlparse(page.url).path.rsplit("/", 1)[1])

    # The record page IS the new draft's, and its supplier is the pre-filled
    # one — the choosing created the right purchase. T4: the draft's supplier
    # renders in the inline header's picker FIELD (its value is the fact; the
    # old read-only text line is gone).
    record = api.get_json(f"/api/purchases/{new_id}")
    assert int(record["purchase"]["supplier_id"]) == supplier_id, (
        "the draft must belong to the supplier the dialog pre-filled"
    )
    expect(page.get_by_role("heading", name="Draft purchase")).to_be_visible()
    expect(page.locator("#record-supplier")).to_have_value(supplier_name)


def test_typing_a_different_supplier_and_pressing_enter_creates_the_typed_one(
    page: Page, api: ApiClient
) -> None:
    """The Enter path (purchases-create-and-header T3, the hazard).

    The dialog arrives pre-filled with the LAST USED supplier; the operator
    types a DIFFERENT exact name and presses Enter inside the text field.
    That submits the picker's OWN form, whose only field is the text input:
    the field's text resolves server-side, so the draft must belong to the
    typed supplier — never the pre-filled one. The stored record is read
    back through the API, so a draft created for the wrong supplier cannot
    pass. (The picker's form once carried a hidden `supplier_id` for the
    pre-filled supplier, which silently won over the typed name — this test
    is the regression proof that the text is the contract.)
    """
    prefilled_name = "Enter Prefilled Supplier"
    prefilled_id = create_supplier(api, prefilled_name)
    create_purchase_draft(api, prefilled_id, payment_type="Cash")
    typed_name = "Enter Typed Supplier"
    typed_id = create_supplier(api, typed_name)

    with page.expect_response(_response_for("/web/purchases")):
        page.goto(f"{api.base_url}/purchases")

    page.locator("button[data-page-action]").click()
    dialog = page.locator("#new-purchase-dialog")
    expect(dialog).to_be_visible()
    picker_input = dialog.locator("#new-purchase-supplier")
    expect(picker_input).to_have_value(prefilled_name)

    # Type the other supplier's exact name and press Enter: the picker's own
    # form posts, the htmx branch answers `HX-Redirect` and the browser lands
    # on the new draft's record.
    picker_input.fill(typed_name)
    # Arm the expectation around the Enter press: if the response lands
    # before the listener is armed, expect_response times out waiting for an
    # event that already happened.
    with page.expect_response(_response_for("/web/purchases", method="POST")):
        picker_input.press("Enter")
    page.wait_for_url("**/purchases/*")
    new_id = int(urlparse(page.url).path.rsplit("/", 1)[1])

    record = api.get_json(f"/api/purchases/{new_id}")
    assert int(record["purchase"]["supplier_id"]) == typed_id, (
        "the draft must belong to the supplier the operator typed, "
        f"not the pre-filled one ({prefilled_name})"
    )
    expect(page.get_by_role("heading", name="Draft purchase")).to_be_visible()
    # T4: the draft's supplier renders in the inline header's picker FIELD —
    # the typed name, not the pre-filled one.
    expect(page.locator("#record-supplier")).to_have_value(typed_name)


def test_a_typed_fragment_of_a_supplier_name_renders_the_search_results_and_picking_scopes_the_draft(
    page: Page, api: ApiClient
) -> None:
    """The only test that resolves a supplier the way the browser does: by a fragment.

    Every supplier test above types an EXACT name and submits — a journey that
    never needs the results list, which is how the picker shipped with a
    search that returned nothing in a browser: htmx sends the triggering
    input's value under its own name (`supplier_name`), the endpoint read
    only `q`, so the query was always empty and the fragment never rendered.\
    The Rust endpoint tests call with `?q=` and pass regardless; only a real
    keystroke on the real name can see the defect.

    So this test types a FRAGMENT (not the whole name, so exact resolution can
    never be the way forward) and applies the negative gate twice — a supplier
    whose name shares nothing with the fragment must NOT appear in the
    results, in the creation dialog and on the record header alike, so the
    list is proven FILTERED by the query, not merely rendered. It then walks
    both journeys the list enables: a clicked result in the creation dialog
    creates the scoped draft, and the same fragment-driven pick on a draft's
    header re-scopes the document. The stored records are read back through
    the API so a UI-only assertion cannot pass.
    """
    homonym_a = "Almacén del Norte"
    homonym_b = "Almacén del Sur"
    non_matching = "Ferretería Central"
    id_a = create_supplier(api, homonym_a)
    id_b = create_supplier(api, homonym_b)
    create_supplier(api, non_matching)
    fragment = "Almacén"

    with page.expect_response(_response_for("/web/purchases")):
        page.goto(f"{api.base_url}/purchases")
    page.locator("button[data-page-action]").click()
    dialog = page.locator("#new-purchase-dialog")
    expect(dialog).to_be_visible()

    # Type a fragment, not a name: the result list is the only way forward.
    dialog.locator("#new-purchase-supplier").fill(fragment)
    results = dialog.locator("#supplier-search-results")
    expect(results).to_contain_text(homonym_a)
    expect(results).to_contain_text(homonym_b)
    # The negative gate: the query was APPLIED, not ignored into the roster —
    # a supplier that cannot match the fragment stays out of the results.
    expect(results).not_to_contain_text(non_matching)
    expect(results).not_to_contain_text("No suppliers match")

    # Click the match: the result's own form posts the picker's caller target,
    # so the draft creation must not depend on the field's text at all.
    with page.expect_response(_response_for("/web/purchases", method="POST")):
        results.locator("button", has_text=homonym_a).click()
    page.wait_for_url("**/purchases/*")
    new_id = int(urlparse(page.url).path.rsplit("/", 1)[1])

    record = api.get_json(f"/api/purchases/{new_id}")
    assert int(record["purchase"]["supplier_id"]) == id_a, (
        "the clicked result (not the fragment text) must scope the draft"
    )
    expect(page.get_by_role("heading", name="Draft purchase")).to_be_visible()
    expect(page.locator("#record-supplier")).to_have_value(homonym_a)

    # The header edit walks the SAME fragment-driven journey on the record:
    # click the field, type a fragment, click a result, the header re-scopes
    # to the pick.
    supplier_field = page.locator("#record-supplier")
    # The header field arrives PRE-FILLED with the document's supplier, and
    # clicking it must clear that default — the operator clicks to search for
    # a DIFFERENT one. The click clears only the value: no debounced search
    # fires for the emptied field, so the results region still shows its
    # initial status rather than an empty-query reply.
    expect(supplier_field).to_have_value(homonym_a)
    supplier_field.click()
    expect(supplier_field).to_have_value("")
    expect(page.locator("#supplier-search-results")).to_contain_text(
        "Type a supplier name."
    )
    supplier_field.fill(fragment)
    header_results = page.locator("#supplier-search-results")
    expect(header_results).to_contain_text(homonym_b)
    # The negative gate on the record page: the header's query filters too.
    expect(header_results).not_to_contain_text(non_matching)
    expect(header_results).not_to_contain_text("No suppliers match")

    with page.expect_response(
        _response_for(f"/web/purchases/{new_id}/header", method="POST")
    ):
        header_results.locator("button", has_text=homonym_b).click()

    stored = api.get_json(f"/api/purchases/{new_id}")
    assert int(stored["purchase"]["supplier_id"]) == id_b, stored
    assert id_a != id_b, "the test is not discriminating: both clicks picked the same supplier"
    expect(page.locator("#record-supplier")).to_have_value(homonym_b)


def test_editing_a_draft_header_in_place_updates_the_document(page: Page, api: ApiClient) -> None:
    """The inline header (purchases-create-and-header T4, AC5).

    On a draft's record page the supplier, the purchase date, the supplier
    invoice no and the notes are editable where the work happens — the
    always-visible header form that replaced the Edit header dialog. The
    operator changes the supplier through the picker's field (its typed name
    resolves server-side, exactly as the creation flow does), edits the
    invoice and the notes, and saves through the existing
    `POST /web/purchases/{id}/header` — now driven by each field's own
    `change` (receiving-flow T2), with no button. Nothing is swapped: the
    fields still show the operator's values precisely because the response
    replaces nothing, and the stored document is read back through the API
    so a UI-only update cannot pass. The due date stays untouched: it is
    decided at confirm, and a header edit must not clear it.
    """
    stored_name = "Header Stored Supplier"
    stored_id = create_supplier(api, stored_name)
    changed_name = "Header Changed Supplier"
    changed_id = create_supplier(api, changed_name)
    purchase_id = create_purchase_draft(
        api, stored_id, payment_type="Credit", due_date="2024-06-01"
    )

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    # The draft's header form arrives pre-filled with the document's values.
    supplier_field = page.locator("#record-supplier")
    expect(supplier_field).to_have_value(stored_name)
    expect(page.locator("#record-invoice-no")).to_have_value("")

    # Edit all three in place: supplier, invoice, notes. Each fill blurs the
    # previous field, so the invoice's `change` fires when the notes field
    # takes focus; the notes' own `change` waits for a deliberate blur.
    supplier_field.fill(changed_name)
    page.locator("#record-invoice-no").fill("INV-E2E-4")
    page.locator("#record-notes").fill("changed in place")

    # Arm the expectation around the save: a listener armed after the action
    # races the response and times out (the T3 lesson). Filling the notes
    # blurred the invoice, whose `change` already posted an earlier save
    # that carried the notes still empty; this deliberate blur is the LAST
    # post, the one that persists all three edits together.
    with page.expect_response(
        _response_for(f"/web/purchases/{purchase_id}/header", method="POST")
    ):
        page.locator("#record-notes").press("Tab")

    # The fields carry the operator's values — nothing replaced them: the
    # auto-save swaps nothing, so focus and the typed values survive.
    expect(page.locator("#record-supplier")).to_have_value(changed_name)
    expect(page.locator("#record-invoice-no")).to_have_value("INV-E2E-4")
    expect(page.locator("#record-notes")).to_have_value("changed in place")

    # Stored state, read back through the API.
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    assert int(stored["purchase"]["supplier_id"]) == changed_id, stored
    assert stored["purchase"]["supplier_invoice_no"] == "INV-E2E-4", stored
    assert stored["purchase"]["notes"] == "changed in place", stored
    # The due date is decided at confirm: a header edit must not touch it.
    assert stored["purchase"]["due_date"] == "2024-06-01", stored


def test_editing_a_line_inline_keeps_every_id_on_the_page_unique(
    page: Page, api: ApiClient
) -> None:
    """An inline line edit must swap only the money region, not the whole body.

    The add-line form consumes the response with `hx-select="#purchase-record-money"`.
    The inline qty and unit-cost inputs did not select anything, so the whole
    record body — the header included — was inserted as the money region's
    `innerHTML`, and every id on the page came back a second time: two
    `#purchase-header`, two `#purchase-record-inner`, two
    `#purchase-record-money`, and so on.

    Duplicate ids are not cosmetic. They make every id-based lookup ambiguous:
    the row's `hx-include` — `"#line-qty-{id}, #line-cost-{id},
    #line-cost-gross-{id}, #line-cost-basis-{id}"` — would match each of its four
    selectors twice, `hx-target` resolves to whichever comes first, and the
    picker island's `mount()` would mount the duplicated `[data-picker]`. The page
    is corrupt until it is reloaded. This is the same family as the hidden-id
    defect the picker work retired: an id lookup silently resolving to the wrong
    element.

    The selector list is FOUR entries since the cost pair (cost-with-taxes T-C)
    joined the row: the quantity, the net, the supplier's tax-inclusive gross and
    the hidden `cost_basis` that says which of the two costs the operator typed.
    All three inputs share the one PUT and all three must carry the whole set —
    a request that reached the write missing the gross could not be answered,
    and a request that reached it missing the basis would silently prefer the
    net. That is why the row's fields are asserted here as a set, not as an
    attribute that happens to be present.

    The assertion is on the counts rather than on the attributes, because the
    attribute can be present while the behaviour is still wrong; the counts
    cannot. This is also the first browser test of the inline edit at all — the
    Rust suite only pins that the inputs exist in the markup.
    """
    product = create_product(
        api,
        sku="INLINE-SKU",
        name="Inline Widget",
        sale_price="20.00",
        cost_price="5.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    supplier_id = create_supplier(api, "Inline Supplier")
    purchase_id = create_purchase_draft(api, supplier_id, payment_type="Cash")
    add_purchase_line(api, purchase_id, int(product["id"]), qty="2", unit_cost="6.00")

    detail = api.get_json(f"/api/purchases/{purchase_id}")
    line_id = int(detail["lines"][0]["id"])

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    unique_ids = [
        "#purchase-record-inner",
        "#purchase-header",
        "#purchase-header-form",
        "#purchase-record-money",
        "#line-picker",
        f"#line-qty-{line_id}",
        f"#line-cost-{line_id}",
        f"#line-cost-gross-{line_id}",
        f"#line-cost-basis-{line_id}",
    ]
    before = {sel: page.locator(sel).count() for sel in unique_ids}
    assert all(count == 1 for count in before.values()), (
        f"the page must start with one of each id: {before}"
    )

    qty = page.locator(f"#line-qty-{line_id}")
    expect(qty).to_have_value("2")
    # Arm the expectation around the action: a listener armed after it races the
    # response and times out (the T3 lesson recorded in the receiving-desk doc).
    with page.expect_response(
        _response_for(f"/web/purchases/{purchase_id}/lines/{line_id}", "PUT")
    ):
        qty.fill("7")
        qty.press("Tab")  # the trigger is `change delay:400ms`

    # Let the swap settle before counting. Counting first would race it.
    page.wait_for_timeout(800)

    after = {sel: page.locator(sel).count() for sel in unique_ids}
    assert after == before, (
        "an inline line edit duplicated the record body, so every id on the page "
        f"is now ambiguous: before={before} after={after}"
    )

    # And the edit actually landed: the guard must not pass by doing nothing.
    expect(page.locator(f"#line-qty-{line_id}")).to_have_value("7")
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    assert Decimal(str(stored["lines"][0]["qty"])) == Decimal("7"), stored


def _seed_header_draft(api: ApiClient) -> int:
    """A draft with a supplier and no lines: enough for the identity fields."""
    supplier_id = create_supplier(api, "Autosave Supplier")
    return create_purchase_draft(api, supplier_id, payment_type="Cash")


def test_a_header_field_alone_saves_without_a_button(
    page: Page, api: ApiClient
) -> None:
    """The identity fields save themselves, so the Save header button is gone.

    Only the supplier field saved on its own before this: the picker's post
    carries the whole header through `hx-include`, while the date, the invoice
    number and the notes were plain inputs with no `hx-trigger` at all. So the
    button was the only way to save those three alone — it read as redundant
    because half the header really did auto-save, which is worse than either
    extreme.
    """
    purchase_id = _seed_header_draft(api)
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    assert (
        page.locator("#purchase-header-form")
        .get_by_role("button", name="Save header")
        .count()
        == 0
    ), "the Save header button must be gone"

    invoice = page.locator("#record-invoice-no")
    expect(invoice).to_have_value("")
    with page.expect_response(
        _response_for(f"/web/purchases/{purchase_id}/header", "POST")
    ):
        invoice.fill("INV-AUTO")
        invoice.press("Tab")  # `change` fires on blur

    page.wait_for_timeout(400)
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    assert stored["purchase"]["supplier_invoice_no"] == "INV-AUTO", stored


def test_an_auto_saved_header_does_not_steal_focus(
    page: Page, api: ApiClient
) -> None:
    """The save must not swap the record, or it eats the operator's next field.

    This is the whole reason the answer is empty-plus-trigger instead of the
    record body. A full-record swap on every blur would replace the field the
    operator is tabbing into and pull focus back to the one they just left.
    """
    purchase_id = _seed_header_draft(api)
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    invoice = page.locator("#record-invoice-no")
    invoice.fill("INV-FOCUS")
    invoice.press("Tab")

    # The operator keeps working while the save is in flight.
    notes = page.locator("#record-notes")
    notes.click()
    notes.fill("typed while the save lands")
    page.wait_for_timeout(900)

    # The save must have actually happened, or "focus survived" means nothing:
    # with no auto-save at all nothing swaps and focus survives trivially. This
    # assertion is what makes the focus check a real gate.
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    assert stored["purchase"]["supplier_invoice_no"] == "INV-FOCUS", (
        f"the save did not happen, so the focus check is vacuous: {stored}"
    )

    assert page.evaluate("document.activeElement.id") == "record-notes", (
        "the auto-save stole focus: "
        f"activeElement={page.evaluate('document.activeElement.id')}"
    )
    expect(notes).to_have_value("typed while the save lands")


def test_an_auto_saved_header_does_not_spam_the_notice(
    page: Page, api: ApiClient
) -> None:
    """One save per blur must not raise one global notice per blur.

    `base.html` announces `"<action> saved"` for every successful form post
    that carries a `data-action`. That is right for a button pressed once and
    wrong for a field left once: the operator would get a stack of notices
    while filling in three fields. The form opts out of the generic success
    notice and shows a subtle indicator instead; its FAILURES must still be
    named, so the `data-action` stays.
    """
    purchase_id = _seed_header_draft(api)
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    invoice = page.locator("#record-invoice-no")
    with page.expect_response(
        _response_for(f"/web/purchases/{purchase_id}/header", "POST")
    ):
        invoice.fill("INV-QUIET")
        invoice.press("Tab")
    page.wait_for_timeout(500)

    expect(page.locator("#notice")).to_be_empty()


def test_an_empty_required_date_posts_nothing_and_says_so_inline(
    page: Page, api: ApiClient
) -> None:
    """Clearing the date to retype it must not fire a save that answers 400.

    The date is `required`. With a save on every blur, emptying the field to
    type a new one would post an empty date, the route would refuse it, and the
    operator would be told the document failed to save for the crime of
    retyping a date. The guard is client-side, and the hint is next to the
    field rather than in the global notice region.
    """
    purchase_id = _seed_header_draft(api)
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    posted: list[str] = []
    page.on(
        "request",
        lambda request: posted.append(request.url)
        if "/header" in request.url
        else None,
    )

    date = page.locator("#record-purchase-date")
    date.fill("")
    date.press("Tab")
    page.wait_for_timeout(800)

    assert posted == [], f"an empty date posted a save: {posted}"
    expect(page.locator("#record-purchase-date-error")).to_be_visible()
    expect(page.locator("#notice")).to_be_empty()

    # And the document is untouched: the stored date is still the seeded one.
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    assert stored["purchase"]["purchase_date"], stored


def test_editing_a_line_inline_keeps_focus_and_updates_the_totals(
    page: Page, api: ApiClient
) -> None:
    """An inline edit keeps the operator's place and moves the derived numbers.

    The inline qty/cost inputs swap `#purchase-record-money` with `outerHTML`,
    so editing one replaces the whole money region — including the inputs
    themselves. Two things could therefore break, and neither does:

    1. **Focus survives.** The swap destroys the focused field, but htmx
       restores focus for elements that carry an id, and these inputs do. The
       operator tabs from a quantity to the next field and stays there.
    2. **The totals follow.** The row subtotal and the running total are
       re-rendered from the new quantity.

    Written while scoping the entry-row island task, to find out whether that
    task had any behaviour behind it. It measured both, both held, and the task
    was dropped. It stays as the regression guard for behaviour that had **no
    browser test at all** before — the same coverage gap that let the
    duplicate-region defect live two days.
    """
    first = create_product(
        api,
        sku="INLINE-A",
        name="Inline Alpha",
        sale_price="20.00",
        cost_price="5.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    second = create_product(
        api,
        sku="INLINE-B",
        name="Inline Beta",
        sale_price="20.00",
        cost_price="5.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    supplier_id = create_supplier(api, "Inline Rows Supplier")
    purchase_id = create_purchase_draft(api, supplier_id, payment_type="Cash")
    add_purchase_line(api, purchase_id, int(first["id"]), qty="2", unit_cost="6.00")
    add_purchase_line(api, purchase_id, int(second["id"]), qty="1", unit_cost="6.00")

    detail = api.get_json(f"/api/purchases/{purchase_id}")
    by_product = {int(line["product_id"]): int(line["id"]) for line in detail["lines"]}
    alpha_line = by_product[int(first["id"])]

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    qty = page.locator(f"#line-qty-{alpha_line}")
    cost = page.locator(f"#line-cost-{alpha_line}")
    expect(qty).to_have_value("2")

    def snapshot(label: str) -> None:
        print(
            f"\n[{label}] active={page.evaluate('document.activeElement.id')!r}"
            f" row_subtotal={page.locator(f'#purchase-line-{alpha_line} td.text-expense').inner_text()!r}"
            f" total={page.locator('#purchase-record-money div.text-2xl').first.inner_text()!r}",
            flush=True,
        )

    snapshot("before")

    qty.fill("5")
    # The operator moves on to the next field. This is what fires `change` on
    # the quantity and, with `delay:400ms`, schedules the PUT.
    cost.click()
    snapshot("right after moving focus")

    page.wait_for_timeout(1400)  # debounce + response + swap
    snapshot("after the swap settles")

    # 1. The operator is still where they left off.
    assert page.evaluate("document.activeElement.id") == f"line-cost-{alpha_line}", (
        "the inline edit destroyed the focused field: "
        f"active={page.evaluate('document.activeElement.id')!r}"
    )

    # 2. The derived numbers followed the edit (5 x 6.00 = 30.00, plus 1 x 6.00).
    expect(page.locator(f"#purchase-line-{alpha_line} td.text-expense")).to_contain_text(
        "30.00"
    )
    expect(page.locator("#purchase-record-money div.text-2xl").first).to_contain_text(
        "36.00"
    )


def test_a_refused_inline_line_edit_reverts_and_stores_nothing(
    page: Page, api: ApiClient
) -> None:
    """A value the domain refuses must not stay in the field or in the document.

    `purchase.html` carries hand-written rollback glue for this: on
    `htmx:responseError` it restores `elt.value = elt.defaultValue`, the value
    the last render wrote. That is state ownership in the page shell, and it was
    the last candidate for the entry-row island task.

    Measuring it settled that task instead of refactoring it. htmx does issue
    the PUT for an HTML-invalid quantity despite the `min` constraint, the route
    refuses it, the glue reverts the field, and nothing is stored. All three
    parts are asserted here, because the glue is only worth keeping if the whole
    path works.
    """
    product = create_product(
        api,
        sku="ROLLBACK-A",
        name="Rollback Alpha",
        sale_price="20.00",
        cost_price="5.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    supplier_id = create_supplier(api, "Rollback Supplier")
    purchase_id = create_purchase_draft(api, supplier_id, payment_type="Cash")
    add_purchase_line(api, purchase_id, int(product["id"]), qty="2", unit_cost="6.00")

    detail = api.get_json(f"/api/purchases/{purchase_id}")
    line_id = int(detail["lines"][0]["id"])

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    puts: list[str] = []
    page.on(
        "request",
        lambda request: puts.append(f"{request.method} {request.url}")
        if request.method == "PUT"
        else None,
    )

    qty = page.locator(f"#line-qty-{line_id}")
    expect(qty).to_have_value("2")

    for bad in ["-5", "0"]:
        qty.fill(bad)
        page.locator(f"#line-cost-{line_id}").click()
        page.wait_for_timeout(700)

    # The request did leave the browser: the HTML `min` does not stop htmx, so
    # the server really is the one refusing. Without this the test could pass on
    # a build where the browser silently blocked the value and the glue never ran.
    assert puts, "htmx issued no PUT, so the rollback path was never reached"

    # The field does not keep a value the domain rejected.
    expect(qty).to_have_value("2")

    # And nothing was written.
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    assert Decimal(str(stored["lines"][0]["qty"])) == Decimal("2"), stored


# ---------------------------------------------------------------------------
# Bidirectional cost entry (cost-with-taxes T-C)
#
# Two fields for one figure, and whichever the operator types into is the input.
# The server computes every number here — the entry row's preview asks it to
# solve the other side, and the inline edit's PUT answers with the record body
# re-read from storage — so what these tests assert is that the page asks, and
# that what comes back is the exact figure rather than a rounded neighbour.
# ---------------------------------------------------------------------------

_PREVIEW_PATH_SUFFIX = "/lines/cost"


def _cost_entry_page(api: ApiClient, *, sku: str, name: str, cost: str = "5.00"):
    """A draft with one product carrying a 21% tax, and nothing on the line yet.

    21% is the whole arithmetic: 5,00 nets to 6,05 gross and back with no drift,
    and 0,03 is a figure NO net grosses to at that rate — the per-contribution
    rounding leaves a flat step there. Both numbers come from that one rate, so
    the reachable and the refused case are the same fixture.
    """
    product = create_product(
        api,
        sku=sku,
        name=name,
        sale_price="25.00",
        cost_price=cost,
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    tax = api.post_json(
        "/api/taxes",
        {"code": f"IVA21-{sku}", "name": "IVA 21%", "rate": "21", "is_active": True},
    )
    api.post_json(f"/api/products/{product_id}/taxes", {"tax_id": int(tax["id"])})
    supplier_id = create_supplier(api, f"Cost Supplier {sku}")
    purchase_id = create_purchase_draft(api, supplier_id, payment_type="Cash")
    return product_id, purchase_id


def test_a_gross_typed_into_the_entry_row_fills_the_net_and_stores_it(
    page: Page, api: ApiClient
) -> None:
    """Type the supplier's figure; the net appears, and the line is stored at it.

    This is the whole promise of the feature in one interaction, and the reason
    the net is asserted to the CENT rather than to "something near 5": 6.05 at
    21% is the gross of exactly 5.00, and a page that computed the net itself
    would land on 5.00 too — so the stored line, not the visible field, is what
    proves the server did the work.
    """
    product_id, purchase_id = _cost_entry_page(
        api, sku="COSTIN-A", name="Cost entry alpha"
    )
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    net = page.locator("#line-unit-cost")
    gross = page.locator("#line-unit-cost-gross")
    expect(net).to_have_value("")
    expect(gross).to_have_value("")

    # The product is chosen the way a receiving desk does it — the text goes in
    # and Enter carries it — and the cost is typed beside it.
    page.locator("#product-picker").fill("COSTIN-A")

    # ONE preview request, and the net fills from its answer. The count is
    # asserted because a pair that answers each other is the failure this shape
    # is built to prevent, and "the net is right" alone would not notice it.
    previews: list[str] = []
    page.on(
        "request",
        lambda request: previews.append(request.url)
        if _PREVIEW_PATH_SUFFIX in urlparse(request.url).path
        else None,
    )
    gross.fill("6.05")
    gross.press("Tab")
    # "5" and not "5.00": `Decimal` carries no trailing zeros, so a SOLVED figure
    # renders at the scale it happens to hold while a TYPED one renders at the
    # scale the operator typed. Both parse and store as the same number — the
    # read-back below is what proves that — and the app renders every derived
    # figure this way, the ladder's included.
    expect(net).to_have_value("5")
    assert len(previews) == 1, f"one answer per field, not a loop: {previews}"

    # And the gross the operator typed is still theirs: the answer goes into the
    # OTHER field.
    expect(gross).to_have_value("6.05")

    # Adding the line stores the solved net, exactly.
    page.locator("#product-picker").press("Enter")
    row = page.locator(f"#purchase-line-{product_id}")
    expect(row).to_have_count(1)
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    line = next(line for line in stored["lines"] if int(line["product_id"]) == product_id)
    assert Decimal(str(line["unit_cost"])) == Decimal("5.00"), line
    # 21% of the line's own net subtotal (1 x 5.00 = 5.00) — the tax the WRITE
    # froze, which is what the gross was derived against.
    assert Decimal(str(line["tax_total"])) == Decimal("1.05"), line


def test_a_net_typed_into_the_entry_row_fills_the_gross(page: Page, api: ApiClient) -> None:
    """The mirror runs the other way too, and on the same contract.

    5,00 at 21% grosses to 6,05 — which is the ladder's `cost_total` arithmetic
    and the same `calculate_line_taxes` a line write will run. Asserting the
    gross here is what keeps the two directions honest about each other: a
    "mirror" that only ever solved gross to net would pass the first test.
    """
    _product_id, purchase_id = _cost_entry_page(api, sku="COSTIN-B", name="Cost entry beta")
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    net = page.locator("#line-unit-cost")
    gross = page.locator("#line-unit-cost-gross")

    page.locator("#product-picker").fill("COSTIN-B")
    net.fill("5.00")
    net.press("Tab")

    expect(gross).to_have_value("6.05")
    expect(net).to_have_value("5.00")


def test_a_gross_the_operators_own_figure_keeps_what_they_typed(
    page: Page, api: ApiClient
) -> None:
    """The loop guard: an answer must never overwrite the field being typed in.

    The trap is a slow answer, not a fast one. The operator types a net, walks
    away to the gross field and starts THEIR figure, and the first answer lands
    in between — carrying the counterpart of the net they typed, which belongs
    in the gross field, which is now full of something else. The
    `data-last-edited` marker is the only thing that knows they have moved on.

    The first response is HELD so the ordering is the one the guard exists for
    rather than a race the debounce usually hides — and the test then WAITS for
    both answers to have landed. Without that wait the whole first half passes
    vacuously: nothing arriving is indistinguishable from an answer arriving and
    being correctly discarded, so a page that never mirrored anything would pass
    it too. The clean interaction afterwards is the other half of that: it proves
    the writer is alive.
    """
    _product_id, purchase_id = _cost_entry_page(
        api, sku="COSTIN-C", name="Cost entry gamma"
    )
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    net = page.locator("#line-unit-cost")
    gross = page.locator("#line-unit-cost-gross")
    page.locator("#product-picker").fill("COSTIN-C")

    # Hold the FIRST preview until released and pass every later one straight
    # through. Holding all of them would also hold the clean interaction at the
    # end, which is not what is under test.
    release: list[bool] = [False]
    held: list[int] = [0]

    def _hold(route) -> None:
        held[0] += 1
        if held[0] > 1:
            route.continue_()
            return
        while not release[0]:
            page.wait_for_timeout(20)
        route.continue_()

    page.route(f"**{_PREVIEW_PATH_SUFFIX}*", _hold)

    # Count what actually reached the page, so "the late answer arrived and was
    # discarded" is a fact this test establishes rather than an assumption.
    answers: list[str] = []
    page.on(
        "response",
        lambda response: answers.append(response.text())
        if _PREVIEW_PATH_SUFFIX in urlparse(response.url).path
        else None,
    )

    net.fill("5.00")
    net.press("Tab")
    page.wait_for_timeout(600)
    # The operator moves on before the first answer is allowed to land.
    gross.fill("7.00")
    release[0] = True

    # The operator is still IN the gross field — no blur, so no second question
    # and no second answer. The one answer that lands is the counterpart of the
    # net they typed earlier, and it names the field they are now standing in.
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and not answers:
        page.wait_for_timeout(25)
    assert held[0] >= 1, "no preview was ever held, so the ordering was never tested"
    assert answers, (
        "the held answer must actually LAND for this test to say anything: an answer that "
        f"never arrives leaves 'the operator's figure survived' true of a page that never "
        f"mirrors at all"
    )

    # Both of their figures survive. The late answer named the gross field — 6.05,
    # the counterpart of the 5.00 they typed first — and the gross field is where
    # they are now, mid-figure, so the answer is dropped and the net keeps what
    # they typed.
    expect(gross).to_have_value("7.00")
    expect(net).to_have_value("5.00")

    # The writer is demonstrably ALIVE, so the assertions above are about the
    # guard and not about a page that never mirrors anything: a clean interaction
    # now fills the other field exactly as the first interaction did.
    page.unroute(f"**{_PREVIEW_PATH_SUFFIX}*")
    gross.fill("3.63")
    gross.press("Tab")
    expect(net).to_have_value("3")


def test_a_gross_that_is_the_gross_of_no_net_says_so_and_stores_nothing(
    page: Page, api: ApiClient
) -> None:
    """0,03 at 21% is the gross of no net. The page says so, and adds nothing.

    A rounded approximation would be the wrong kind of success: it would store a
    net whose gross is not the figure on the invoice, and the operator would
    have no way to see the difference. So the refusal is the assertion, and the
    document must be untouched afterwards.
    """
    product_id, purchase_id = _cost_entry_page(
        api, sku="COSTIN-D", name="Cost entry delta"
    )
    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    net = page.locator("#line-unit-cost")
    gross = page.locator("#line-unit-cost-gross")
    page.locator("#product-picker").fill("COSTIN-D")
    gross.fill("0.03")
    gross.press("Tab")

    refusal = page.locator("[data-cost-refusal]")
    expect(refusal).to_be_visible()
    expect(refusal).to_contain_text("no net cost grosses to this cost")
    # The net the operator had (none) is not invented, and the gross they typed
    # is still theirs to correct.
    expect(gross).to_have_value("0.03")
    expect(net).to_have_value("")

    # And the write refuses the same figure rather than rounding it.
    page.locator("#product-picker").press("Enter")
    page.wait_for_timeout(800)
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    assert stored["lines"] == [], stored["lines"]
    assert int(product_id) not in {
        int(line["product_id"]) for line in stored["lines"]
    }


def test_a_gross_typed_into_an_inline_edit_stores_the_solved_net(
    page: Page, api: ApiClient
) -> None:
    """The same two figures, one row down, with no preview at all.

    The inline row is not mirrored in the browser: the PUT's answer IS the record
    body, re-rendered from storage, so the server is that row's mirror. The test
    therefore asserts the re-rendered PAIR and the stored line — a client that
    filled the net field optimistically and never posted the gross would leave
    the stored cost at the old figure, and that is what the read-back catches.
    """
    product_id, purchase_id = _cost_entry_page(
        api, sku="COSTIN-E", name="Cost entry epsilon"
    )
    add_purchase_line(api, purchase_id, product_id, qty="2", unit_cost="5.00")
    line_id = int(api.get_json(f"/api/purchases/{purchase_id}")["lines"][0]["id"])

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    net = page.locator(f"#line-cost-{line_id}")
    gross = page.locator(f"#line-cost-gross-{line_id}")
    expect(net).to_have_value("5.00")
    expect(gross).to_have_value("")

    put = _response_for(f"/web/purchases/{purchase_id}/lines/{line_id}", "PUT")
    with page.expect_response(put):
        gross.fill("6.05")
        gross.press("Tab")

    # The re-rendered pair comes from STORAGE, so the net is the solved figure
    # the write just stored, and the gross is back to its stored rendering.
    page.wait_for_timeout(500)
    expect(net).to_have_value("5")
    expect(gross).to_have_value("")

    stored = api.get_json(f"/api/purchases/{purchase_id}")
    line = next(line for line in stored["lines"] if int(line["id"]) == line_id)
    assert Decimal(str(line["unit_cost"])) == Decimal("5.00"), line

    # And the other direction on the same row: edit the net, and it is the net
    # that is stored. A stale basis marker would make the write solve from the
    # gross again and store 5.00 a second time.
    with page.expect_response(put):
        net.fill("4.00")
        net.press("Tab")
    stored = api.get_json(f"/api/purchases/{purchase_id}")
    line = next(line for line in stored["lines"] if int(line["id"]) == line_id)
    assert Decimal(str(line["unit_cost"])) == Decimal("4.00"), line


def test_a_refused_gross_edit_reverts_both_fields_and_stores_nothing(
    page: Page, api: ApiClient
) -> None:
    """The rollback covers the gross field too, and the net never moved.

    `purchase.html` reverts a refused inline edit with `elt.value =
    elt.defaultValue`. The gross input is a full participant in that PUT, so the
    refused figure is cleared back to the field's stored rendering and the stored
    line keeps the cost it had — which is the same guarantee the quantity-only
    case has always had, now that a second figure can be the one refused.
    """
    product_id, purchase_id = _cost_entry_page(
        api, sku="COSTIN-F", name="Cost entry zeta"
    )
    add_purchase_line(api, purchase_id, product_id, qty="2", unit_cost="5.00")
    line_id = int(api.get_json(f"/api/purchases/{purchase_id}")["lines"][0]["id"])

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    puts: list[str] = []
    page.on(
        "request",
        lambda request: puts.append(f"{request.method} {request.url}")
        if request.method == "PUT"
        else None,
    )

    net = page.locator(f"#line-cost-{line_id}")
    gross = page.locator(f"#line-cost-gross-{line_id}")
    # The 400 is the answer, so the response is waited for rather than slept
    # through: a listener armed after the action races the debounce and times out.
    with page.expect_response(
        _response_for(f"/web/purchases/{purchase_id}/lines/{line_id}", "PUT")
    ):
        gross.fill("0.03")
        net.click()  # the blur is what fires `change delay:400ms`
    page.wait_for_timeout(300)

    assert puts, "htmx issued no PUT, so the rollback path was never reached"
    expect(page.locator("#notice [data-notice='error']")).to_contain_text(
        "no net cost grosses to this cost"
    )
    expect(gross).to_have_value("")
    expect(net).to_have_value("5.00")

    stored = api.get_json(f"/api/purchases/{purchase_id}")
    line = next(line for line in stored["lines"] if int(line["id"]) == line_id)
    assert Decimal(str(line["unit_cost"])) == Decimal("5.00"), line
    assert Decimal(str(line["qty"])) == Decimal("2"), line
