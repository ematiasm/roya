"""Slice T6 (cost-freshness): the apply-cost journey, driven in a browser.

A draft purchase line whose cost is higher than the product's stored cost shows
a warning with both numbers and an "Apply to product" button; clicking it writes
the line's cost into the product, which recomputes a markup-derived sale price.

The Rust suite already covers the pieces (the warning row, the gated route, the
recomputation); what only a browser can prove is the journey an operator lives:
the warning is actually visible on the page, the button actually fires the
htmx post, and the product genuinely ends up updated. The stored-state
assertion reads the product through the API, so the test fails if the button
posts and the write silently stops firing — the browser half proves the
interaction, the API half proves the effect.

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


def _line_id(api: ApiClient, purchase_id: int, product_id: int) -> int:
    """The id of the line the seed created for one product."""
    detail = api.get_json(f"/api/purchases/{purchase_id}")
    return int(
        next(
            line["id"]
            for line in detail["lines"]
            if int(line["product_id"]) == product_id
        )
    )


def test_a_stale_draft_line_warns_with_both_numbers_and_applying_updates_the_product(
    page: Page, api: ApiClient
) -> None:
    """A higher line cost warns, and Apply to product really moves the product.

    The journey an operator lives: see the disagreement on the purchase page,
    click the button, and the product's stored cost becomes the line's cost
    while the markup re-derives the sale price. The failure this catches beyond
    the Rust route tests: the warning rendering but the click dying (htmx
    broken, the route swapping the wrong fragment), which would leave the
    operator with a button that does nothing — and the assertion below reads
    the product through the API, so a no-op write cannot pass.
    """
    markup = "50"
    product = create_product(
        api,
        sku="APPLY-SKU-01",
        name="Apply Widget",
        # A manual price that no derivation could produce by accident: if the
        # seed's own derived price were not 15.00, the later recomputation
        # would be indistinguishable from the manual price sticking.
        sale_price="99.00",
        cost_price="10.00",
        markup_pct=markup,
        stock="5",
        min_stock="1",
        max_stock="50",
    )
    product_id = int(product["id"])

    # The markup must be ACTIVE at seed time, or the recomputation the click
    # triggers proves nothing: assert the stored price is already the derived
    # one before any click happens.
    starting = api.get_json(f"/api/products/{product_id}")
    assert Decimal(str(starting["sale_price"])) == _derived_sale_price("10.00", markup), (
        starting
    )
    starting_price = Decimal(str(starting["sale_price"]))

    supplier_id = create_supplier(api, "Apply Supplier")
    purchase_id = create_purchase_draft(api, supplier_id)
    add_purchase_line(
        api, purchase_id, product_id, qty="2", unit_cost="12.00"
    )
    line_id = _line_id(api, purchase_id, product_id)

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    # Both numbers in the warning's own sentence, as one exact text node: a
    # line row repeating an amount cannot satisfy it, and either half dropping
    # silently breaks the match.
    warning = page.get_by_text("line cost $12.00 • stored $10.00", exact=True)
    expect(warning).to_be_visible()
    apply_button = page.get_by_role("button", name="Apply to product")
    expect(apply_button).to_be_visible()

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
    # Asserting the recomputed price against the derived value (12.00 × 1.5)
    # fails the test the moment the derivation stops firing.
    stored = api.get_json(f"/api/products/{product_id}")
    assert Decimal(str(stored["cost_price"])) == Decimal("12.00"), stored
    expected_price = _derived_sale_price("12.00", markup)
    assert expected_price != starting_price, (
        "the test is not discriminating: the derived price equals the starting one"
    )
    assert Decimal(str(stored["sale_price"])) == expected_price, stored
    assert Decimal(str(stored["markup_pct"])) == Decimal(markup), stored


def test_a_draft_line_whose_cost_equals_the_stored_cost_offers_no_apply(
    page: Page, api: ApiClient
) -> None:
    """Equal costs are the fresh state: no warning, and therefore no button.

    The failure this pins is the disagreement gate collapsing to always-true:
    the UI would then offer the action on every line even when there is
    nothing to apply, and a click would rewrite the product with its own
    stored cost while claiming freshness work happened.
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
    purchase_id = create_purchase_draft(api, supplier_id)
    add_purchase_line(api, purchase_id, product_id, qty="1", unit_cost="8.00")

    page.goto(f"{api.base_url}/purchases/{purchase_id}")

    # `stale cost` appears nowhere else in the templates but the two warning
    # renders (this one and the product drawer's badge), so a page-level
    # absence is discriminating; the button is the action the warning carries,
    # so its absence follows.
    expect(page.get_by_text("stale cost", exact=True)).to_have_count(0)
    expect(page.get_by_role("button", name="Apply to product")).to_have_count(0)
