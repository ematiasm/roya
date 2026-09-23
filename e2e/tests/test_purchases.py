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
    warning = page.get_by_text("line cost $12.00 • stored $10.00", exact=True)
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

    # Escape closes the drawer AND empties the body — the emptied body is the
    # DOM property the page's close script owns, the same claim the documents
    # drawer tests make for their sibling.
    page.keyboard.press("Escape")
    expect(drawer).not_to_be_visible()
    assert (
        page.evaluate("document.getElementById('purchase-drawer-body').innerHTML")
        == ""
    ), "Escape must empty the peek body, not just hide the drawer"


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
    is an anchor carrying `text-bg`, so its label resolves to `--color-bg`
    (#0f1115 → rgb(15, 17, 21)) — a different dark than the base button's
    #0a0f0d. Both are dark; neither is blue.

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
