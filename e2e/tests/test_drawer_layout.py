"""The shared drawer's two modes, measured at the widths where they differ.

The drawer is ONE component now: five modules carry the same ``.drawer`` panel
inside the same ``.drawer-split`` frame, and one controller in ``base.html``
drives all five. The claim that follows from that is a claim about REUSE, and a
claim about reuse is only worth what it is measured at. This spec measures it at
both modes, for all five modules, at the widths where the two differ — which is
also the only way to show the reuse is real rather than five coincidences.

It exists because of a deliberate decision that has a cost worth naming. The
suite's default viewport is 1280x720, which is BELOW the component's ``90rem``
(1440px) split threshold. That is on purpose — splitting at ``lg`` would have
inverted every existing ``not_to_be_visible()`` drawer assertion in the suite
into a vacuous pass — but it means the default width exercises exactly ONE mode.
So before this spec the rail had no functional test at all and was fingerprinted
only by a visual baseline captured at 1440x900: one pixel above the very
threshold under test. The overlay's full-bleed branch, the ``min(100%, …)`` bound
below the 25rem floor that makes the detail the whole screen on a phone, had
never been exercised by anything.

Three things this spec holds itself to:

* **A width is stated, never assumed.** Every test sets its viewport with
  ``set_viewport_size`` and the number it sets is named by a constant that says
  what the number is. The suite's own default is left alone, because 1280
  staying below the threshold is the property that keeps the other specs honest.
* **The assertion is about what the browser computed, not about a name.** A
  test that reads ``position`` and ``display`` survives renaming the component,
  re-templat-ing it, or moving the rule to a different layer. A test that reads
  a class string pins the spelling and calls it coverage.
* **A layout test that passes with the split on OR off is worse than no test**,
  because it manufactures confidence. Each test was checked by breaking the
  stylesheet or the controller and watching the test go red, and the docstring on
  each names the mutations that kill it — including the one mutation that
  survives, because a test that claims a discrimination it does not have is the
  same defect wearing a different hat.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable
from urllib.parse import urlparse

import pytest
from playwright.sync_api import Page, expect

from helpers import (
    ApiClient,
    add_purchase_line,
    create_confirmed_credit_sale,
    create_customer,
    create_product,
    create_purchase_draft,
    create_supplier,
    e2e_copy,
)

# ---------------------------------------------------------------------------
# The widths, named for what they are so a reader never has to look the
# threshold up. 1440 is 90rem, and the split happens AT it, not above it: the
# baseline's 1440x900 capture already sits on that knife edge, which is why it
# is a baseline and not a functional test.
# ---------------------------------------------------------------------------

RAIL_VIEWPORT = {"width": 1440, "height": 900}
ONE_PIXEL_UNDER_RAIL_VIEWPORT = {"width": 1439, "height": 900}
OVERLAY_VIEWPORT = {"width": 1280, "height": 720}
PHONE_VIEWPORT = {"width": 360, "height": 740}


# ---------------------------------------------------------------------------
# The five modules, as data
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class _Seeded:
    """One seeded row: its id, and the two texts that prove it rendered.

    Two markers, not one, because the two halves of the journey show different
    things and conflating them hides a real class of failure. The LIST row and
    the panel BODY are separate swaps by separate endpoints, so a click that
    opened the right panel with the wrong fragment satisfies one marker and
    fails the other. Only purchases separates them — its list row prints the
    supplier while its body prints the line's product — which is precisely why
    the field is not a convenience.
    """

    record_id: int
    list_marker: str
    body_marker: str


@dataclass(frozen=True)
class _Module:
    """One module's drawer, addressed the way that module's own spec addresses it.

    ``row_selector`` and ``detail_path`` are both keyed on the endpoint the row
    control calls, which is the house idiom for these five lists and the reason
    the five row shapes can differ at all: products rows carry a per-record id,
    parties rows carry none, documents' rows are a button that names a document
    family, and a purchase row is an ``<a>`` that keeps its ``href``. Addressing
    the row by what it FETCHES is what makes one table cover all five.
    """

    name: str
    page_path: str
    list_path: str
    list_id: str
    drawer_id: str
    body_id: str
    row_selector: Callable[[int], str]
    detail_path: Callable[[int], str]
    seed: Callable[[ApiClient, str], _Seeded]


def _seeded_product(api: ApiClient, tag: str) -> _Seeded:
    """A tracked product: both halves print its name."""
    product = create_product(
        api,
        sku=f"{tag}-SKU",
        name=f"{tag} Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    return _Seeded(int(product["id"]), f"{tag} Widget", f"{tag} Widget")


def _seeded_customer(api: ApiClient, tag: str) -> _Seeded:
    """A customer: the list prints the name and the statement prints it back."""
    return _Seeded(create_customer(api, f"{tag} Buyer"), f"{tag} Buyer", f"{tag} Buyer")


def _seeded_supplier(api: ApiClient, tag: str) -> _Seeded:
    """A supplier: the list prints the name and the drawer prints it above the balance."""
    return _Seeded(create_supplier(api, f"{tag} Supplier"), f"{tag} Supplier", f"{tag} Supplier")


def _seeded_purchase(api: ApiClient, tag: str) -> _Seeded:
    """A draft purchase with one line, so the row exists and the money table has text.

    This is the one module whose two halves show DIFFERENT names, and the
    difference is real rather than incidental: the list row is the purchase's
    counterpart (the supplier), while the body is the purchase's contents, and
    the body's marker is the LINE's product name. The header's supplier name
    sits inside one branch of the record template, and a marker that depends on
    a branch is a marker that can quietly stop being there.
    """
    product = create_product(
        api,
        sku=f"{tag}-SKU",
        name=f"{tag} Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    supplier_id = create_supplier(api, f"{tag} Supplier")
    purchase_id = create_purchase_draft(api, supplier_id)
    add_purchase_line(api, purchase_id, int(product["id"]), qty="1", unit_cost="5.00")
    return _Seeded(purchase_id, f"{tag} Supplier", f"{tag} Widget")


def _seeded_document(api: ApiClient, tag: str) -> _Seeded:
    """A confirmed credit sale, the document feed's own proof that a row exists.

    Seeded through the confirmed path the documents spec already uses, because
    "a draft sale is in the feed" is an assumption and "a confirmed sale is in
    the feed" is what the suite has been running against.
    """
    customer_id = create_customer(api, f"{tag} Buyer")
    product_id = int(
        create_product(
            api,
            sku=f"{tag}-SKU",
            name=f"{tag} Widget",
            stock="10",
            min_stock="1",
            max_stock="100",
        )["id"]
    )
    sale_id = create_confirmed_credit_sale(api, customer_id, product_id)
    return _Seeded(sale_id, f"{tag} Buyer", f"{tag} Buyer")


MODULES: tuple[_Module, ...] = (
    _Module(
        name="products",
        page_path="/products",
        list_path="/web/products",
        list_id="product-list-inner",
        drawer_id="product-drawer",
        body_id="product-drawer-body",
        row_selector=lambda rid: (
            f'#product-{rid} button[hx-get="/web/products/detail/{rid}"]'
        ),
        detail_path=lambda rid: f"/web/products/detail/{rid}",
        seed=_seeded_product,
    ),
    _Module(
        name="purchases",
        page_path="/purchases",
        list_path="/web/purchases",
        list_id="purchase-list-inner",
        drawer_id="purchase-drawer",
        body_id="purchase-drawer-body",
        # The purchase row is an <a>: it keeps its href as the no-JavaScript
        # fallback and htmx takes the click instead, so the control is the
        # anchor itself and its hx-get names a DOCUMENT fragment, not a
        # purchase one.
        row_selector=lambda rid: (
            f'#purchase-{rid}[hx-get="/web/documents/detail/purchase/{rid}"]'
        ),
        detail_path=lambda rid: f"/web/documents/detail/purchase/{rid}",
        seed=_seeded_purchase,
    ),
    _Module(
        name="customers",
        page_path="/customers",
        list_path="/web/customers",
        list_id="customer-list-inner",
        drawer_id="customer-drawer",
        body_id="customer-drawer-body",
        row_selector=lambda rid: (
            f'#customer-list-inner button[hx-get="/web/customers/detail/{rid}"]'
        ),
        detail_path=lambda rid: f"/web/customers/detail/{rid}",
        seed=_seeded_customer,
    ),
    _Module(
        name="documents",
        page_path="/documents",
        list_path="/web/documents",
        list_id="document-list-inner",
        drawer_id="document-drawer",
        body_id="document-drawer-body",
        # The feed's rows span two families, so the row names which one it is;
        # the sale family is the one seeded here.
        row_selector=lambda rid: (
            f'#document-list button[hx-get="/web/documents/detail/sale/{rid}"]'
        ),
        detail_path=lambda rid: f"/web/documents/detail/sale/{rid}",
        seed=_seeded_document,
    ),
    _Module(
        name="suppliers",
        page_path="/suppliers",
        list_path="/web/suppliers",
        list_id="supplier-list-inner",
        drawer_id="supplier-drawer",
        body_id="supplier-drawer-body",
        row_selector=lambda rid: (
            f'#supplier-list-inner button[hx-get="/web/suppliers/{rid}/detail"]'
        ),
        detail_path=lambda rid: f"/web/suppliers/{rid}/detail",
        seed=_seeded_supplier,
    ),
)

PRODUCTS = MODULES[0]


# ---------------------------------------------------------------------------
# Small navigation helpers (the parties drawer pattern)
# ---------------------------------------------------------------------------


def _response_for(path: str, method: str = "GET"):
    """Predicate matching one request path and method, query string ignored."""

    def matches(response) -> bool:
        return urlparse(response.url).path == path and response.request.method == method

    return matches


def _open_module_list(page: Page, api: ApiClient, module: _Module, marker: str) -> None:
    """Open a module's page and wait out the re-fetch its ``load`` trigger fires.

    The server renders the list and htmx immediately asks for it again; acting
    before that second response lands risks the swap replacing the row
    mid-click. The seeded row's own text is what proves the list that answered
    is the one the click is about to happen in.
    """
    with page.expect_response(_response_for(module.list_path)):
        page.goto(f"{api.base_url}{module.page_path}")
    expect(page.locator(f"#{module.list_id}")).to_contain_text(marker)


def _open_module_row(page: Page, module: _Module, seeded: _Seeded) -> None:
    """Click a row's control and wait for the fragment it fetches.

    The control's ``hx-get`` and the fragment path are the same in all five
    modules, so one value both finds the control and synchronises on its
    response — no sleep, and no second wait for a swap that may already be over.
    """
    with page.expect_response(_response_for(module.detail_path(seeded.record_id))):
        page.locator(module.row_selector(seeded.record_id)).click()


# ---------------------------------------------------------------------------
# What the browser computed
# ---------------------------------------------------------------------------


def _panel_position(page: Page, drawer_id: str) -> str:
    """The panel's computed ``position``.

    This is the one property that separates the two modes. The overlay is
    ``fixed`` and out of flow; the rail is ``static`` and a grid track. It is
    read from the cascade rather than from a class, so renaming the component or
    restating the rule in a different layer cannot break it.
    """
    return page.evaluate(
        "id => getComputedStyle(document.getElementById(id)).position", drawer_id
    )


def _frame_display(page: Page, drawer_id: str) -> str:
    """The computed ``display`` of the panel's own frame — its parent element.

    Found structurally, as ``parentElement``, and never by the ``.drawer-split``
    class: the assertion is about the two-column frame becoming a grid, and a
    test that pinned the class name would be pinning the spelling of the thing
    rather than the thing.
    """
    return page.evaluate(
        "id => getComputedStyle(document.getElementById(id).parentElement).display",
        drawer_id,
    )


def _panel_width(page: Page, drawer_id: str) -> float:
    """The panel's rendered width, in CSS pixels, as the layout resolved it."""
    return page.evaluate(
        "id => document.getElementById(id).getBoundingClientRect().width", drawer_id
    )


def _viewport_width(page: Page) -> int:
    """The width the page believes it has, which is what ``100%`` resolves against.

    ``documentElement.clientWidth`` rather than ``window.innerWidth``: a classic
    scrollbar takes space out of the layout viewport, and a fixed panel's
    percentage width resolves against the space that is left, not against the
    space the scrollbar sits in. Reading the other number would make this test
    measure the scrollbar.
    """
    return page.evaluate("() => document.documentElement.clientWidth")


def _has_data_open(page: Page, drawer_id: str) -> bool:
    """Whether the controller has marked the panel open.

    ``hasAttribute`` and not the attribute's value: the controller sets
    ``data-open`` to the EMPTY STRING, so reading the value would make "open"
    and "not open" indistinguishable and every assertion on it vacuous.
    """
    return page.evaluate(
        "id => document.getElementById(id).hasAttribute('data-open')", drawer_id
    )


def _body_text(page: Page, body_id: str) -> str:
    """The drawer's body text, trimmed, read off the live DOM."""
    return page.evaluate(
        "id => document.getElementById(id).textContent.trim()", body_id
    )


def _empty_state_child_count(page: Page, body_id: str) -> int:
    """How many direct ``.empty`` children the drawer's body holds."""
    return page.evaluate(
        "id => document.querySelectorAll('#' + id + ' > .empty').length", body_id
    )


def _assert_body_holds_the_empty_state(page: Page, module: _Module, *, where: str) -> None:
    """Assert the drawer's body holds the shared empty state and no detail.

    Covers both producers of that state: the server on first paint, and the
    shared controller after a close. The claim is NOT "the panel is hidden" —
    ``not_to_be_visible()`` already says that and cannot tell an emptied body
    from a full one. It is "the detail is gone and the panel says so", which
    matters most at the rail, where the panel is permanent and a body with
    nothing in it is what the operator stares at.

    The child COUNT is what keeps this honest: a leftover detail fragment that
    happened to carry the same sentence would otherwise pass on the text alone.
    """
    count = _empty_state_child_count(page, module.body_id)
    assert count == 1, (
        f"{where}: {module.name} body must hold exactly one .empty child and no "
        f"detail, found {count}"
    )
    text = _body_text(page, module.body_id)
    assert text == e2e_copy("drawer_empty"), (
        f"{where}: {module.name} body must hold no detail, only the empty state; "
        f"held {text!r}"
    )


# ---------------------------------------------------------------------------
# 1. The threshold itself
# ---------------------------------------------------------------------------


def test_the_split_threshold_is_a_boundary_at_exactly_1440(
    page: Page, api: ApiClient
) -> None:
    """1439px overlays; 1440px is a column. One page, one DOM, one click never made.

    This is the highest-value test in the file, because the threshold is a
    LITERAL in the stylesheet and a number in a comment — `90rem` is stated in
    exactly two places, neither of which a Rust test can execute, because a
    custom property cannot be read in a media condition (the `--drawer-split-at`
    token form parses, builds, and silently never matches). So the number was
    documentation. This makes it a fact.

    The shape is what gives it its power: the page is LOADED at 1439 and then
    only the viewport changes. Same DOM, no re-navigation, no click, so the one
    variable is the width — which is exactly the variable the component claims
    to read. Both directions are asserted, because a test that only checks the
    rail still passes on a threshold set too low.

    What is asserted is the computed `position` of the panel and the computed
    `display` of its frame: `fixed`/`block` below, `static`/`grid` at the
    threshold. Renaming `.drawer` or `.drawer-split`, or moving either rule into
    another layer, leaves this test green — deliberately, since the claim is
    about the layout and not about the spelling.

    Discriminating: raising the stylesheet's threshold to an unreachable width
    (999rem) fails the 1440 half; LOWERING it to 70rem fails the 1439 half.
    """
    seeded = PRODUCTS.seed(api, "THRESHOLD")
    page.set_viewport_size(ONE_PIXEL_UNDER_RAIL_VIEWPORT)
    _open_module_list(page, api, PRODUCTS, seeded.list_marker)

    panel = page.locator(f"#{PRODUCTS.drawer_id}")

    # One pixel under the threshold: an overlay, closed, that no click has opened.
    expect(panel).not_to_be_visible()
    assert _panel_position(page, PRODUCTS.drawer_id) == "fixed", (
        "below the threshold the panel must be a fixed overlay"
    )
    assert _frame_display(page, PRODUCTS.drawer_id) == "block", (
        "below the threshold the frame must not be a grid"
    )

    # The only thing that changed is the viewport.
    page.set_viewport_size(RAIL_VIEWPORT)

    # At the threshold the panel is a column, and it is there with no click at
    # all. `to_be_visible` is also the synchronisation: the panel is displayed
    # only from inside the threshold's media query, so once it is visible the
    # query has been evaluated and the reads below cannot be stale.
    expect(panel).to_be_visible()
    assert _panel_position(page, PRODUCTS.drawer_id) == "static", (
        "at and above the threshold the panel must be in flow, not fixed"
    )
    assert _frame_display(page, PRODUCTS.drawer_id) == "grid", (
        "at and above the threshold the frame must be the two-column grid"
    )


# ---------------------------------------------------------------------------
# 2. The rail, all five modules
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("module", MODULES, ids=lambda m: m.name)
def test_the_rail_is_there_with_no_click_at_1440(
    page: Page, api: ApiClient, module: _Module
) -> None:
    """Above the threshold every module's panel is a permanent column, unopened.

    The rail is the whole point of the split — an overlay and the list compete
    for the same width, so one of them can only win, while a track and a column
    do not — and it had NO functional test before this. It was fingerprinted
    only by the visual baseline, whose 1440x900 viewport sits exactly on the
    threshold, so a one-pixel rounding change would have flipped the whole
    baseline from "the rail renders" to "an overlay renders" and no functional
    test would have noticed.

    Three claims per module, and the middle one is the load-bearing: the panel
    is visible, its computed `position` is not `fixed` (so it is IN the page and
    the list is beside it, not covered by it), and it holds the designed empty
    state rather than nothing. `not_to_have_attribute` is the fourth and it is
    what makes "permanent" mean permanent: the controller never opened this
    panel, so its visibility cannot be the result of a `data-open` left over
    from anything.

    Discriminating: raising the threshold to 999rem fails every case (the panel
    is `display: none`); deleting the in-flow `.drawer` rule fails every case
    (the panel stays `fixed` and closed).
    """
    seeded = module.seed(api, f"RAIL-{module.name}")
    page.set_viewport_size(RAIL_VIEWPORT)
    _open_module_list(page, api, module, seeded.list_marker)

    panel = page.locator(f"#{module.drawer_id}")

    expect(panel).to_be_visible()
    assert _panel_position(page, module.drawer_id) != "fixed", (
        f"{module.name}: above the threshold the panel must be in flow, so the list "
        f"is not covered by it"
    )
    assert not _has_data_open(page, module.drawer_id), (
        f"{module.name}: the rail is permanent, so its panel must be visible with "
        f"nothing having opened it"
    )
    _assert_body_holds_the_empty_state(page, module, where="on first paint at 1440")


# ---------------------------------------------------------------------------
# 3. The overlay, all five modules
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("module", MODULES, ids=lambda m: m.name)
def test_the_overlay_opens_on_a_row_and_escape_closes_it_at_1280(
    page: Page, api: ApiClient, module: _Module
) -> None:
    """Below the threshold every module's panel is a closed overlay that opens and closes.

    1280 is the suite's own default and the width the rest of the drawer specs
    already run at, so this is the overlay's baseline behaviour stated once per
    module instead of incidentally many times: nothing to see, then the row's
    own record, then Escape takes it away and leaves the empty state behind.

    The viewport is set explicitly even though it equals the default, and that is
    not redundancy: the claim in the name is a claim about 1280, and a test that
    inherited its width from a fixture would quietly stop being about 1280 the
    day somebody changed that fixture. The suite's default is NOT changed —
    1280 staying below the threshold is what keeps every other drawer assertion
    in the suite meaningful.

    "Real content" is asserted two ways, because either alone is weak: the
    body's text must carry the seeded record's own marker, AND the body must no
    longer hold a `.empty` child. A panel that opened onto a leftover empty state
    would satisfy a visibility check and fail here.

    Discriminating, measured: LOWERING the threshold to 70rem fails every case,
    because 1280 would then be above it and the panel would already be a visible
    column before any click. RAISING it to 999rem does NOT fail these — that was
    tried, and all five stayed green, which is correct rather than a gap: at 1280
    the panel is an overlay under either threshold, so the split is not one of
    the things this test claims and a mutation of it should not reach here. The
    mutation that does kill the after-Escape half is removing the controller's
    empty-state re-insertion.
    """
    seeded = module.seed(api, f"OVERLAY-{module.name}")
    page.set_viewport_size(OVERLAY_VIEWPORT)
    _open_module_list(page, api, module, seeded.list_marker)

    panel = page.locator(f"#{module.drawer_id}")

    # Closed, and closed for the right reason: it is a fixed overlay nobody
    # opened, not a hidden column.
    expect(panel).not_to_be_visible()
    assert _panel_position(page, module.drawer_id) == "fixed", (
        f"{module.name}: below the threshold the panel must be a fixed overlay"
    )
    _assert_body_holds_the_empty_state(page, module, where="on first paint at 1280")

    _open_module_row(page, module, seeded)

    expect(panel).to_be_visible()
    expect(page.locator(f"#{module.body_id}")).to_contain_text(seeded.body_marker)
    assert _empty_state_child_count(page, module.body_id) == 0, (
        f"{module.name}: an open panel must be holding the record, not the empty state"
    )

    page.keyboard.press("Escape")

    expect(panel).not_to_be_visible()
    _assert_body_holds_the_empty_state(page, module, where="after Escape at 1280")


# ---------------------------------------------------------------------------
# 4. The full-bleed branch
# ---------------------------------------------------------------------------


def test_the_overlay_is_full_bleed_at_360(page: Page, api: ApiClient) -> None:
    """At 360px the open panel spans the whole viewport: the detail IS the screen.

    This is the untested branch of the width ladder, and the reason the ladder
    is written the way it is. `.drawer` computes
    ``width: min(100%, clamp(25rem, 42vw, 40rem))`` — and at 360px the clamp
    alone would answer 400px, which is 40px WIDER than the space it overlays, so
    the panel would hang off the left edge of a phone and cover the page's own
    gutter. The `min(100%, …)` is the bound that makes the ladder continuous and
    the panel never larger than the space it overlays. That `min()` had no test.

    The claim is a RENDERED SIZE, not a declaration: the panel's measured width
    against the width the page believes it has. A `min()` that is present in the
    source and defeated by something else downstream still fails here, which is
    the point of measuring.

    Discriminating: dropping the `min(100%, …)` bound — leaving the bare clamp —
    makes the panel 400px at a 360px viewport and this fails on the number.
    """
    seeded = PRODUCTS.seed(api, "FULLBLEED")
    page.set_viewport_size(PHONE_VIEWPORT)
    _open_module_list(page, api, PRODUCTS, seeded.list_marker)

    _open_module_row(page, PRODUCTS, seeded)

    panel = page.locator(f"#{PRODUCTS.drawer_id}")
    expect(panel).to_be_visible()
    expect(page.locator(f"#{PRODUCTS.body_id}")).to_contain_text(seeded.body_marker)

    measured = _panel_width(page, PRODUCTS.drawer_id)
    available = _viewport_width(page)
    assert measured == available, (
        "at 360px the open panel must span the whole viewport — the detail is the "
        f"screen — but it measured {measured}px against {available}px available"
    )
    # The floor the clamp would have answered on its own, stated so the
    # assertion above cannot pass by coincidence: 25rem is 400px, and at a 360px
    # viewport the panel is NOT that wide.
    assert available < 400, (
        f"this test is not discriminating: the viewport is {available}px, which is "
        "not below the 25rem clamp floor, so the full-bleed branch is not the one "
        "being measured"
    )


# ---------------------------------------------------------------------------
# 5. The ✕ at the rail
# ---------------------------------------------------------------------------


def test_the_rail_survives_its_own_close_button_at_1440(
    page: Page, api: ApiClient
) -> None:
    """At the rail, ✕ clears the SELECTION and leaves the column standing.

    This is the most quietly load-bearing behaviour in the controller and the
    easiest to regress, because it is a difference between two widths rather
    than a behaviour anyone tests directly. `closeDrawer` empties the body
    always, but collapses the panel only while the panel is overlaying; on the
    rail the panel is a column, so there is nothing to collapse and the ✕
    clears the selection instead. The failure this pins is a controller that
    emptied nothing: the operator presses ✕ to clear a selection and is left
    staring at the record they just dismissed, with no way to tell that the ✕
    did anything at all.

    So the assertion is a PAIR, and only the pair means anything: the panel is
    STILL VISIBLE and the body is back to the empty state. Asserting only the
    emptiness would pass on a collapsed panel; asserting only the visibility
    would pass on a panel still showing the record. The ✕ is addressed by the
    controller's own `data-drawer-close` opt-in attribute, which is the
    contract it binds, not by the glyph or the label.

    Discriminating, measured: removing `closeDrawer`'s empty-state re-insertion
    fails this test. The OTHER half of the mechanism does not, and the reason is
    worth writing down rather than discovering later: deleting the controller's
    rail guard — making it collapse the panel at every width — leaves this test
    GREEN, because above the threshold the stylesheet's `.drawer` rule already
    sets `display: flex` unconditionally, after the `data-open` rule and at equal
    specificity. So the rail's permanence is guaranteed by the STYLESHEET, and
    the controller's `drawerIsRail` branch is redundancy that no DOM can observe.
    The behaviour is right either way; only the redundancy is invisible, and this
    test pins the behaviour rather than the redundancy.
    """
    seeded = PRODUCTS.seed(api, "RAILCLOSE")
    page.set_viewport_size(RAIL_VIEWPORT)
    _open_module_list(page, api, PRODUCTS, seeded.list_marker)

    panel = page.locator(f"#{PRODUCTS.drawer_id}")
    expect(panel).to_be_visible()

    _open_module_row(page, PRODUCTS, seeded)

    # A row is open: the body holds the record, not the empty state.
    expect(page.locator(f"#{PRODUCTS.body_id}")).to_contain_text(seeded.body_marker)
    assert _empty_state_child_count(page, PRODUCTS.body_id) == 0, (
        "the row must be open before the ✕ can mean anything"
    )

    page.locator(f"#{PRODUCTS.drawer_id} [data-drawer-close]").click()

    # The selection is cleared…
    _assert_body_holds_the_empty_state(page, module=PRODUCTS, where="after ✕ at 1440")
    # …and the column is still there, because the rail is permanent.
    expect(panel).to_be_visible()
    assert _panel_position(page, PRODUCTS.drawer_id) != "fixed", (
        "the panel must still be the in-flow column the rail put it in"
    )
