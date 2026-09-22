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

from decimal import ROUND_HALF_UP, Decimal
from urllib.parse import urlparse

from playwright.sync_api import Page, expect

from helpers import (
    ApiClient,
    add_purchase_line,
    confirm_purchase,
    create_product,
    create_purchase_draft,
    create_supplier,
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
