"""The redesigned products screen: header buttons, modals and the product drawer.

The redesign moved creation into two modal dialogs (category and product), removed
the four permanent right-rail cards (New Category, New Product, Stock Movement,
REST API), and turned each row name into a slide-over drawer carrying the inline
edit form, the per-supplier cost satellite and the stock movement form. These
tests drive that screen in a real browser and assert what the browser actually
rendered: the page holds the list and nothing the drawer owns, the modals create
through the real endpoints and their refreshes land, and the drawer opens, saves,
records costs and movements in place.

They deliberately do not re-check business rules -- the Rust suite owns validation,
cost shifting and stock arithmetic. What is asserted here is UX wiring and
rendering: the fragment arrives, the drawer opens, the swap lands, the value
shows. The regression tests in the drawer section cover the three user-reported
filter/drawer defects: a catalogue filter that only applied on blur, a Refresh
button that ignored the active filter, and a save that left the drawer open and
(under a matching filter) hid the edited row.

Unlike the party lists, product rows carry a per-record id (`#product-{id}`), so a
row control is scoped by that id and additionally proven bound to the record by
the endpoint its name button calls.

The opt-in probe at the bottom writes full-page screenshots of the redesigned
states for a human to look at; it is skipped unless
``ROYA_E2E_PRODUCTS_SCREENSHOT_PROBE=1``.
"""

from __future__ import annotations

import os
from urllib.parse import urlparse

import pytest
from playwright.sync_api import Page, expect

from conftest import ARTIFACTS_ROOT
from helpers import ApiClient, create_product, create_supplier, record_supplier_cost

# The env gate for the design-screenshot probe, the same one-shot shape as the
# parties probe: opt-in, skipped by default, no effect on a normal run.
SCREENSHOT_PROBE_ENV = "ROYA_E2E_PRODUCTS_SCREENSHOT_PROBE"

_PRODUCTS_PAGE = "/products"
_PRODUCTS_LIST = "/web/products"
_PRODUCTS_INNER = "product-list-inner"


# ---------------------------------------------------------------------------
# Small navigation helpers
# ---------------------------------------------------------------------------


def _response_for(path: str, method: str = "GET"):
    """Predicate matching one request path and method, query string ignored."""

    def matches(response) -> bool:
        return urlparse(response.url).path == path and response.request.method == method

    return matches


def _drawer_trigger(page: Page, product_id: int):
    """The row name wired to one product, addressed by its ``hx-get``.

    The redesigned rows carry a per-record id (`#product-{id}`), so the row id
    scopes the lookup and the detail endpoint in the ``hx-get`` both finds the
    right button and proves the row is bound to the record under test.
    """
    return page.locator(
        f'#product-{product_id} button[hx-get="/web/products/detail/{product_id}"]'
    )


def _edit_form(page: Page):
    """The drawer's inline Save product form, inside the swapped fragment."""
    return page.locator('#product-detail-inner form[hx-post="/web/products/edit"]')


def _open_products_list(page: Page, api: ApiClient, *, name: str) -> None:
    """Open the products page and wait out the re-fetch its ``load`` trigger fires.

    The server renders the list and htmx immediately asks for it again; acting
    before that second response lands risks the swap replacing the row mid-click.
    """
    with page.expect_response(_response_for(_PRODUCTS_LIST)):
        page.goto(f"{api.base_url}{_PRODUCTS_PAGE}")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).to_contain_text(name)


def _open_product_drawer(page: Page, product_id: int) -> None:
    """Click a row name and wait for its detail fragment before asserting.

    The name's ``hx-get`` and the fragment path are the same, so one argument
    both finds the control and synchronizes on its response.
    """
    with page.expect_response(_response_for(f"/web/products/detail/{product_id}")):
        _drawer_trigger(page, product_id).click()


# ---------------------------------------------------------------------------
# Page structure
# ---------------------------------------------------------------------------


def test_products_page_keeps_the_list_and_drops_the_old_cards(
    page: Page, api: ApiClient
) -> None:
    """The page is the list plus the two modal buttons and the empty drawer shell.

    Seeded with one product so a clickable row name must exist. The sharpest
    assertions are the absences: the old right rail's New Product, Stock Movement
    and REST API cards must not render anywhere (the shared sidebar still carries
    a ``REST API ↗`` link, so only the cards' own ids and exact headings count).
    """
    product = create_product(
        api,
        sku="PAGE-SKU-01",
        name="Page Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])

    _open_products_list(page, api, name="Page Widget")

    # The list filter bar owns the refresh control, shared with the Clear
    # anchor inside #product-filters; the Low Stock card carries its own
    # identically labelled refresh button by design, so scoping to the filter
    # form is what keeps the lookup unambiguous.
    filter_bar = page.locator("#product-filters")
    expect(filter_bar.get_by_role("button", name="↻ Refresh")).to_be_visible()
    expect(filter_bar.get_by_text("Clear", exact=True)).to_be_visible()
    # Title row: the two modal openers stayed behind while the refresh moved
    # into the filter bar.
    title_row = page.get_by_role("heading", name="Products", exact=True).locator("xpath=..")
    expect(title_row.get_by_role("button", name="New category")).to_be_visible()
    expect(title_row.get_by_role("button", name="New product")).to_be_visible()

    # The two dialog shells exist but stay closed until a header button opens them.
    for dialog_id in ("#new-category-dialog", "#new-product-dialog"):
        expect(page.locator(dialog_id)).to_have_count(1)
        expect(page.locator(dialog_id)).not_to_be_visible()

    # The drawer shell is present, closed, and holds nothing yet.
    expect(page.locator("#product-drawer")).to_have_count(1)
    expect(page.locator("#product-drawer")).not_to_be_visible()
    expect(page.locator("#product-drawer-body")).to_have_text("")

    # The list and the Low Stock card remain, and the seeded row's name is the
    # drawer trigger the drawer tests click.
    expect(page.locator("#product-list")).to_be_visible()
    expect(page.locator("#low-stock-section")).to_be_visible()
    expect(_drawer_trigger(page, product_id)).to_be_visible()

    # The old permanent cards are gone. Exact text: the sidebar still renders a
    # "REST API ↗" link, and the new UI legitimately says "New product" and
    # "Stock movement" in other casings, so only the old cards' exact spellings
    # and ids must be absent.
    expect(page.locator("#new-product")).to_have_count(0)
    expect(page.get_by_text("New Product", exact=True)).to_have_count(0)
    expect(page.get_by_text("Stock Movement", exact=True)).to_have_count(0)
    expect(page.get_by_text("REST API", exact=True)).to_have_count(0)


# ---------------------------------------------------------------------------
# Creation modals
# ---------------------------------------------------------------------------


def test_new_product_modal_creates_and_refreshes_the_list(
    page: Page, api: ApiClient
) -> None:
    """The New product button opens the modal; submitting creates the record.

    The created product must land in the list next to the existing one, proving
    the create POST answered with the refreshed list and the ``product-created``
    event closed the modal.
    """
    create_product(
        api,
        sku="EXIST-SKU-02",
        name="Existing Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    _open_products_list(page, api, name="Existing Widget")

    page.get_by_role("button", name="New product").click()
    dialog = page.locator("#new-product-dialog")
    expect(dialog).to_be_visible()
    dialog.locator('input[name="sku"]').fill("MODAL-SKU-02")
    dialog.locator('input[name="name"]').fill("Modal Widget")
    dialog.locator('input[name="sale_price"]').fill("12.50")
    with page.expect_response(_response_for("/web/products", "POST")):
        dialog.get_by_role("button", name="Create product").click()

    listing = page.locator(f"#{_PRODUCTS_INNER}")
    expect(listing).to_contain_text("Modal Widget")
    expect(listing).to_contain_text("Existing Widget")
    expect(dialog).not_to_be_visible()


def test_creating_a_product_hidden_by_the_active_filter_says_so_and_offers_the_way_out(
    page: Page, api: ApiClient
) -> None:
    """A create under an active filter that excludes the new row must say so.

    The decided behaviour (issue #37): the filter stays and the server answers
    with its own notice — naming the created product, saying the active filter
    is keeping it out of the list, with a `Clear filter` way out — swapped out
    of band into the page's `#notice` region.

    The sharp assertion is what the notice region ends up holding: exactly one
    box, the server's, not the generic "Create product saved". A broken version
    would look like either failure mode this design guards against — the
    generic `htmx:afterRequest` notice landing after the out-of-band swap and
    replacing the server box (the htmx swap phase runs before afterRequest, so
    without the base.html precedence guard that is exactly what happens), or
    the notice never arriving because the server skipped the under-filter
    check. A broken membership derivation shows up as the notice while the row
    is actually visible, or the row absent from the list with no notice.
    """
    product = create_product(
        api,
        sku="HIDE-SKU-13",
        name="Visible Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    _open_products_list(page, api, name="Visible Widget")

    # Activate a filter that will exclude the product about to be created: type
    # the term and wait for the debounced fragment response.
    with page.expect_response(_response_for(_PRODUCTS_LIST)):
        page.locator('#product-filters input[name="q"]').press_sequentially("Visible")
    listing = page.locator(f"#{_PRODUCTS_INNER}")
    expect(listing).to_contain_text("Visible Widget")

    page.get_by_role("button", name="New product").click()
    dialog = page.locator("#new-product-dialog")
    expect(dialog).to_be_visible()
    dialog.locator('input[name="sku"]').fill("HIDDEN-SKU-14")
    dialog.locator('input[name="name"]').fill("Hidden Widget")
    dialog.locator('input[name="sale_price"]').fill("12.50")
    with page.expect_response(_response_for("/web/products", "POST")):
        dialog.get_by_role("button", name="Create product").click()

    # The server's notice, not the generic "Create product saved": exactly one
    # box in the region, naming the product and saying why it is not in the
    # list. This is the assertion that pins the precedence guard and the
    # out-of-band transport in a real browser.
    region = page.locator("#notice")
    boxes = region.locator("[data-notice]")
    expect(boxes).to_have_count(1)
    box = boxes.first
    expect(box).to_be_visible()
    expect(box).to_contain_text("Hidden Widget created")
    expect(box).to_contain_text("the active catalogue filter is keeping it out of the list")
    expect(box).not_to_contain_text("Create product saved")

    # The filtered list still shows only the matching rows.
    expect(listing).to_contain_text("Visible Widget")
    expect(listing).not_to_contain_text("Hidden Widget")
    # The filter controls still hold the active filter.
    expect(page.locator('#product-filters input[name="q"]')).to_have_value("Visible")

    # The notice offers the way out: following Clear filter reloads the page
    # without the query, and the new row appears.
    with page.expect_navigation():
        box.get_by_text("Clear filter").click()
    expect(page.locator(f"#{_PRODUCTS_INNER}")).to_contain_text("Hidden Widget")
    expect(
        page.locator(f"#product-{int(product['id'])}")
    ).to_contain_text("Visible Widget")


def test_new_category_modal_creates_and_fills_both_category_selects(
    page: Page, api: ApiClient
) -> None:
    """The New category button opens the modal; the fresh category is selectable.

    Submitting creates through the real endpoint, resets the form, and refetches
    both category selects off ``/web/category-options``. The sharp assertions are
    the two option lists: a category that only reached the database but not the
    filter or the product modal's select would break a user's next action, and the
    modal must keep an empty-value option so "no category" stays selectable.
    """
    create_product(
        api,
        sku="CAT-SKU-03",
        name="Category Widget",
        stock="1",
        min_stock="1",
        max_stock="10",
    )
    _open_products_list(page, api, name="Category Widget")

    page.get_by_role("button", name="New category").click()
    dialog = page.locator("#new-category-dialog")
    expect(dialog).to_be_visible()
    dialog.locator('input[name="name"]').fill("Modal Beverages")
    with page.expect_response(_response_for("/web/categories", "POST")):
        dialog.get_by_role("button", name="Create category").click()

    # The form reset itself (the category dialog stays open by design).
    expect(dialog.locator('input[name="name"]')).to_have_value("")

    # The fresh category is selectable in the filter select…
    expect(
        page.locator("#filter-category option").filter(has_text="Modal Beverages")
    ).to_have_count(1)
    # …and in the product modal's category select…
    expect(
        page.locator("#new-product-category option").filter(has_text="Modal Beverages")
    ).to_have_count(1)
    # …which still offers an empty value for "no category".
    expect(page.locator('#new-product-category option[value=""]')).to_have_count(1)


def test_creating_with_a_markup_derives_the_price_and_locks_the_field(
    page: Page, api: ApiClient
) -> None:
    """A markup derives the price server-side, so the empty price must save.

    With a markup the modal's sale price stays empty (the modal's script drops
    ``required`` the moment the markup has a value, and the server accepts an
    empty price whenever a markup is present — the readonly state is courtesy,
    the handler is the enforcement). Asserting the DERIVED value (15.00 USD from a
    10.00 USD cost and a 50% markup) rather than anything the form submitted
    catches a create that echoed a price back instead of deriving one, and the
    drawer's locked field and hint catch a create whose stored markup the UI
    then lost.
    """
    create_product(
        api,
        sku="MARKUP-SEED-17",
        name="Seed Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    _open_products_list(page, api, name="Seed Widget")

    page.get_by_role("button", name="New product").click()
    dialog = page.locator("#new-product-dialog")
    expect(dialog).to_be_visible()
    dialog.locator('input[name="sku"]').fill("MARKUP-SKU-17")
    dialog.locator('input[name="name"]').fill("Markup Widget")
    dialog.locator('input[name="cost_price"]').fill("10.00")
    # The sale price stays empty on purpose: with a markup the server derives
    # and stores it, and an operator never types one.
    dialog.locator('input[name="markup_pct"]').fill("50")
    with page.expect_response(_response_for("/web/products", "POST")):
        dialog.get_by_role("button", name="Create product").click()

    listing = page.locator(f"#{_PRODUCTS_INNER}")
    expect(listing).to_contain_text("Markup Widget")
    row = listing.locator("> div[id^='product-']").filter(has_text="Markup Widget")
    expect(row).to_have_count(1)
    # The stored price is the derivation: cost 10.00 + 50% markup ⇒ 15.00 USD. No
    # price was submitted, so anything else here is a broken derivation.
    expect(row).to_contain_text("15.00 USD")
    product_id = int(row.get_attribute("id").removeprefix("product-"))

    # The drawer mirrors the stored state: the markup input carries the value,
    # the price input is not editable (readonly renders bare, so the
    # editability check is what the browser state actually is), and the muted
    # hint explains why the field is locked.
    _open_product_drawer(page, product_id)
    form = _edit_form(page)
    expect(form.locator('input[name="markup_pct"]')).to_have_value("50")
    price = form.locator('input[name="sale_price"]')
    expect(price).not_to_be_editable()
    expect(price).to_have_value("15.00")
    expect(page.locator("#product-detail-inner")).to_contain_text(
        "Recalculated from the cost and the markup when you save: 50 %"
    )


def test_second_create_after_a_markup_create_is_not_poisoned_by_the_reset(
    page: Page, api: ApiClient
) -> None:
    """A markup create must not leave the modal unable to take a manual price.

    The modal's script locks the price field the moment a markup is typed; the
    success handler then resets the form, and a reset restores values but fires
    no ``input`` event, so the lock used to survive the reset with the markup
    field already empty again. The poisoned second create could not be typed
    into — the price stayed readonly and empty, the handler answered 400, htmx
    swapped nothing, and the operator saw a silent dead end until they happened
    to touch the markup field. The catch: the second product (no markup) must
    be creatable in the SAME page session, with the price that was typed shown
    in the list; a test that only checked the first create would never see the
    stale lock.
    """
    create_product(
        api,
        sku="POISON-SEED-20",
        name="Poison Seed Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    _open_products_list(page, api, name="Poison Seed Widget")

    dialog = page.locator("#new-product-dialog")
    # First create: WITH a markup and an empty sale price, exactly the create
    # that locks the field and then resets the form. The derived price must
    # land in the list so the seed itself is proven before the second act.
    page.get_by_role("button", name="New product").click()
    expect(dialog).to_be_visible()
    dialog.locator('input[name="sku"]').fill("MARKUP-FIRST-20")
    dialog.locator('input[name="name"]').fill("Markup First Widget")
    dialog.locator('input[name="cost_price"]').fill("10.00")
    dialog.locator('input[name="markup_pct"]').fill("50")
    with page.expect_response(_response_for("/web/products", "POST")):
        dialog.get_by_role("button", name="Create product").click()
    expect(dialog).not_to_be_visible()
    listing = page.locator(f"#{_PRODUCTS_INNER}")
    expect(listing).to_contain_text("Markup First Widget")

    # Second create, same page session, WITHOUT reloading: no markup, so the
    # operator must be able to type a price again.
    page.get_by_role("button", name="New product").click()
    expect(dialog).to_be_visible()
    dialog.locator('input[name="sku"]').fill("MANUAL-SECOND-20")
    dialog.locator('input[name="name"]').fill("Manual Second Widget")
    price_input = dialog.locator('input[name="sale_price"]')
    # The sharp assertion: the reset emptied the markup field, so the price
    # field must be editable again — a stale readonly lock from the markup
    # create fails right here, before the fill that needs it.
    expect(price_input).to_be_editable()
    price_input.fill("19.50")
    with page.expect_response(_response_for("/web/products", "POST")):
        dialog.get_by_role("button", name="Create product").click()

    # The second product is created and appears in the list with the typed
    # price — an empty price would have been rejected with a 400 htmx ignores.
    expect(listing).to_contain_text("Manual Second Widget")
    row = listing.locator("> div[id^='product-']").filter(
        has_text="Manual Second Widget"
    )
    expect(row).to_have_count(1)
    expect(row).to_contain_text("19.50 USD")


# ---------------------------------------------------------------------------
# Drawer
# ---------------------------------------------------------------------------


def test_clicking_a_product_name_opens_the_drawer_with_the_edit_form(
    page: Page, api: ApiClient
) -> None:
    """The name opens the drawer landing the editable fragment, prefilled.

    Asserts the drawer rendered content, not that a request fired: the fragment
    root, the product name and the derived stock line must be there, and the Save
    product form must carry the stored SKU — prefill is the point, because an
    empty form would silently wipe the product's fields on the next save.
    """
    product = create_product(
        api,
        sku="DRAW-SKU-04",
        name="Drawer Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    _open_products_list(page, api, name="Drawer Widget")
    _open_product_drawer(page, product_id)

    expect(page.locator("#product-drawer")).to_be_visible()
    body = page.locator("#product-detail-inner")
    expect(body).to_contain_text("Drawer Widget")
    expect(body).to_contain_text("Stock 10")

    form = body.locator('form[hx-post="/web/products/edit"]')
    expect(form.locator('input[name="sku"]')).to_have_value("DRAW-SKU-04")
    expect(form.locator('input[name="name"]')).to_have_value("Drawer Widget")


def test_editing_a_product_in_the_drawer_updates_drawer_and_list(
    page: Page, api: ApiClient
) -> None:
    """Saving the drawer's edit form closes the drawer and refreshes the list.

    A successful save closes the drawer (the user asked for it: Save is the end of
    the edit flow); the ``product-changed`` trigger must refresh the list behind
    it with the new values, still bound to the same product id. Asserting both the
    closed drawer and the updated row catches a save that only did one of the two.
    """
    product = create_product(
        api,
        sku="EDIT-SKU-05",
        name="Edit Me Widget",
        sale_price="25.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    _open_products_list(page, api, name="Edit Me Widget")
    _open_product_drawer(page, product_id)

    form = _edit_form(page)
    form.locator('input[name="name"]').fill("Edited Widget")
    form.locator('input[name="sale_price"]').fill("29.50")
    with page.expect_response(_response_for("/web/products/edit", "POST")):
        with page.expect_response(_response_for(_PRODUCTS_LIST)):
            form.get_by_role("button", name="Save product").click()

    # The drawer read the swapped fragment before closing: the new values were in
    # it (proven by the list below rendering them), and the drawer itself is now
    # hidden with its body emptied by closeProductDrawer().
    expect(page.locator("#product-drawer")).not_to_be_visible()
    expect(page.locator("#product-drawer-body")).to_have_text("")

    # The list row behind the drawer was refreshed by the trigger too, still bound
    # to the same product id.
    row = page.locator(f"#product-{product_id}")
    expect(row).to_contain_text("Edited Widget")
    expect(row).to_contain_text("29.50 USD")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).not_to_contain_text("Edit Me Widget")


def test_drawer_markup_field_toggles_the_price_editability_live(
    page: Page, api: ApiClient
) -> None:
    """The drawer's markup field must toggle the price field as it is typed.

    The drawer fragment's inline script mirrors the modal's: a markup in the
    field means the server will derive the price on save, so the field locks;
    clearing it hands the price back to the operator. No test exercised the
    script live — the drawer save tests only ever saw its output after the
    fragment was re-rendered from stored state — so a broken toggle (a lock
    that never engages, or a stale readonly that survives the clear) reached
    the operator unseen. Typing and clearing without saving proves all three
    states in one drawer session.
    """
    product = create_product(
        api,
        sku="TOGGLE-SKU-21",
        name="Toggle Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    _open_products_list(page, api, name="Toggle Widget")
    _open_product_drawer(page, product_id)

    form = _edit_form(page)
    markup = form.locator('input[name="markup_pct"]')
    price = form.locator('input[name="sale_price"]')
    # No markup: the operator owns the price.
    expect(price).to_be_editable()
    # Typing a markup locks it — the server would derive the price on save.
    markup.fill("50")
    expect(price).not_to_be_editable()
    # Clearing it hands the price back, with no save and no reload in between.
    markup.fill("")
    expect(price).to_be_editable()


def test_changing_the_markup_in_the_drawer_recalculates_the_price(
    page: Page, api: ApiClient
) -> None:
    """Saving a changed markup must re-derive the price, not just store it.

    The derivation is server-side and happens on save, so a drawer save that
    stored the new markup but skipped the re-derivation would be invisible in
    the drawer (which re-renders the stored price either way after the swap) —
    the list row is the only surface that exposes it. The refreshed row must
    show the price derived from the NEW markup (cost 10.00 + 100% ⇒ 20.00 USD)
    and must have dropped the price the old markup produced.
    """
    product = create_product(
        api,
        sku="MARKUP-SKU-18",
        name="Recalc Widget",
        cost_price="10.00",
        markup_pct="50",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    _open_products_list(page, api, name="Recalc Widget")
    _open_product_drawer(page, product_id)

    form = _edit_form(page)
    # The product arrives with a markup, so the drawer's price input is locked.
    expect(form.locator('input[name="sale_price"]')).not_to_be_editable()
    form.locator('input[name="markup_pct"]').fill("100")
    with page.expect_response(_response_for("/web/products/edit", "POST")):
        with page.expect_response(_response_for(_PRODUCTS_LIST)):
            form.get_by_role("button", name="Save product").click()

    # A successful save closes the drawer and refreshes the list behind it with
    # the newly derived price.
    expect(page.locator("#product-drawer")).not_to_be_visible()
    row = page.locator(f"#product-{product_id}")
    expect(row).to_contain_text("20.00 USD")
    expect(row).not_to_contain_text("15.00 USD")


def test_clearing_the_markup_in_the_drawer_hands_the_price_back_to_the_operator(
    page: Page, api: ApiClient
) -> None:
    """Clearing the markup must keep the last price and make it editable again.

    Two failure modes live in this one save: a clear that also wiped the price
    (the operator would lose the number they were charging), and a drawer that
    stayed readonly after the markup was gone (a stale courtesy state). The
    price must remain exactly what the last derivation stored — NULL markup
    means manual, not zero — and reopening the drawer must show an editable
    price input beside an empty markup field, so the operator can really take
    the price over.
    """
    product = create_product(
        api,
        sku="MARKUP-SKU-19",
        name="Manual Price Widget",
        cost_price="10.00",
        markup_pct="50",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    _open_products_list(page, api, name="Manual Price Widget")
    _open_product_drawer(page, product_id)

    form = _edit_form(page)
    form.locator('input[name="markup_pct"]').fill("")
    with page.expect_response(_response_for("/web/products/edit", "POST")):
        with page.expect_response(_response_for(_PRODUCTS_LIST)):
            form.get_by_role("button", name="Save product").click()

    # The price survived the clear: the refreshed row still shows what the
    # derivation last stored, and a successful save closed the drawer.
    expect(page.locator("#product-drawer")).not_to_be_visible()
    row = page.locator(f"#product-{product_id}")
    expect(row).to_contain_text("15.00 USD")

    # Reopening the drawer shows a manual price again: the input is editable
    # (no stale readonly state) and the markup field is empty.
    _open_product_drawer(page, product_id)
    form = _edit_form(page)
    price = form.locator('input[name="sale_price"]')
    expect(price).to_be_editable()
    expect(price).to_have_value("15.00")
    expect(form.locator('input[name="markup_pct"]')).to_have_value("")


def test_editing_a_product_while_a_matching_filter_is_active_keeps_the_row_visible(
    page: Page, api: ApiClient
) -> None:
    """The reported symptom: an edit under a matching filter must not hide the row.

    The reported sequence was: filter (which only landed on blur), open a product,
    edit its cost price, save — and the product vanished from the list until a
    refresh, because the post-save list refresh silently re-applied the filter the
    user never saw being applied. With the filter matching the edited product, a
    successful save must leave that product's row visible in the list and close
    the drawer.
    """
    product = create_product(
        api,
        sku="FILTER-SKU-09",
        name="Filter Widget",
        cost_price="10.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    create_product(
        api,
        sku="OTHER-SKU-10",
        name="Other Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    _open_products_list(page, api, name="Filter Widget")

    # The filter matches exactly one product, and it is applied without blurring:
    # type the term and wait for the debounced fragment response.
    with page.expect_response(_response_for(_PRODUCTS_LIST)):
        page.locator('#product-filters input[name="q"]').press_sequentially("Filter")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).to_contain_text("Filter Widget")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).not_to_contain_text("Other Widget")
    expect(page.locator('#product-filters input[name="q"]')).to_have_value("Filter")

    # Edit the cost price in the drawer and save; the drawer closes on success.
    _open_product_drawer(page, product_id)
    form = _edit_form(page)
    form.locator('input[name="cost_price"]').fill("18.25")
    with page.expect_response(_response_for("/web/products/edit", "POST")):
        with page.expect_response(_response_for(_PRODUCTS_LIST)):
            form.get_by_role("button", name="Save product").click()

    expect(page.locator("#product-drawer")).not_to_be_visible()
    # The filtered list must still hold the edited product's row — this is the
    # exact step that used to make the product "disappear".
    row = page.locator(f"#product-{product_id}")
    expect(row).to_be_visible()
    expect(row).to_contain_text("Filter Widget")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).not_to_contain_text("Other Widget")


def test_catalogue_filter_applies_while_typing_without_blurring(
    page: Page, api: ApiClient
) -> None:
    """The catalogue filter narrows the list as the term is typed, no blur needed.

    The filter form used `keyup changed delay:300ms`; htmx 1.9.12 initialises the
    `changed` modifier's lastValue from the element carrying the trigger — the
    ``<form>``, whose ``.value`` is undefined — and compares against that same
    undefined value, so the debounced keystroke branch never fired and the filter
    only applied on blur. Typing a term that matches exactly one seeded product
    must narrow the list on its own, and the field must keep the typed value.
    """
    create_product(
        api,
        sku="LIVE-SKU-11",
        name="Live Filter Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    create_product(
        api,
        sku="UNRELATED-SKU-12",
        name="Unrelated Goods",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    _open_products_list(page, api, name="Live Filter Widget")

    field = page.locator('#product-filters input[name="q"]')
    field.click()
    # Keystrokes only: no fill + Tab, no blur. The debounced response lands about
    # 300ms after the last keystroke, so the request is awaited, not assumed.
    with page.expect_response(_response_for(_PRODUCTS_LIST)):
        page.keyboard.type("Live")

    expect(page.locator(f"#{_PRODUCTS_INNER}")).to_contain_text("Live Filter Widget")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).not_to_contain_text("Unrelated Goods")
    expect(page.locator(f"#{_PRODUCTS_INNER} > div[id^='product-']")).to_have_count(1)
    # The field kept what the user typed: the filter the list shows is the filter
    # the field holds.
    expect(field).to_have_value("Live")


def test_refresh_button_respects_the_active_catalogue_filter(
    page: Page, api: ApiClient
) -> None:
    """The list card's Refresh button re-fetches with the active filter applied.

    The button fetched `/web/products` without including ``#product-filters``, so
    it rendered the unfiltered catalogue while the filter controls and the URL
    still showed the filter — the list and its controls disagreed. Refresh must
    carry the filter like the sales and purchases Refresh buttons do, and must not
    rewrite the URL the filter already set.
    """
    create_product(
        api,
        sku="REFRESH-SKU-13",
        name="Refresh Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    create_product(
        api,
        sku="STOCK-SKU-14",
        name="Stocked Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    _open_products_list(page, api, name="Refresh Widget")

    with page.expect_response(_response_for(_PRODUCTS_LIST)):
        page.locator('#product-filters input[name="q"]').press_sequentially("Refresh")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).to_contain_text("Refresh Widget")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).not_to_contain_text("Stocked Widget")

    # The Refresh button inside the filter bar (the Low Stock card owns a
    # second, identically labelled button by design).
    filter_bar = page.locator("#product-filters")
    with page.expect_response(_response_for(_PRODUCTS_LIST)):
        filter_bar.get_by_role("button", name="↻ Refresh").click()

    # The list stayed filtered: one row, the matching one, and the field keeps the
    # term so the controls agree with what the list shows.
    expect(page.locator(f"#{_PRODUCTS_INNER}")).to_contain_text("Refresh Widget")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).not_to_contain_text("Stocked Widget")
    expect(page.locator(f"#{_PRODUCTS_INNER} > div[id^='product-']")).to_have_count(1)
    expect(page.locator('#product-filters input[name="q"]')).to_have_value("Refresh")


def test_drawer_deactivate_under_a_category_filter_keeps_the_filtered_list(
    page: Page, api: ApiClient
) -> None:
    """Deactivating from the drawer must answer the list the operator is looking at.

    Issue #33: the drawer's lifecycle posts rendered the whole catalogue back
    into ``#product-list``; the ``product-changed`` trigger then re-fetched with
    the filter and masked the wrong render, so the operator never saw it — but
    the wasted unfiltered render was one dropped trigger away from a list that
    silently disagrees with its own filter controls, the exact defect the
    Refresh button already had. The drawer forms carry ``#product-filters`` and
    the lifecycle answer honours it: the drawer closes, the list still holds
    exactly the filtered row, and every ``/web/products`` GET issued during the
    flow carries the active filter (collected with ``page.on("request")``).

    Seeding goes through the real JSON API because the shared helper pins
    ``category_id`` to ``None``; two categories make the filter unambiguous.
    """
    cat_alpha = api.post_json("/api/categories", {"name": "Drawer Cat Alpha", "parent_id": None})
    cat_beta = api.post_json("/api/categories", {"name": "Drawer Cat Beta", "parent_id": None})
    product = api.post_json(
        "/api/products",
        {
            "sku": "DEACT-SKU-15",
            "name": "Deactivate Widget",
            "kind": "Product",
            "category_id": cat_alpha["id"],
            "unit": "un",
            "sale_price": "25.00",
            "cost_price": "10.00",
            "track_stock": True,
            "min_stock": "1",
            "max_stock": "100",
            "location": None,
            "notes": None,
        },
    )
    api.post_json(
        "/api/products",
        {
            "sku": "DEACT-OTHER-16",
            "name": "Deactivate Other Widget",
            "kind": "Product",
            "category_id": cat_beta["id"],
            "unit": "un",
            "sale_price": "25.00",
            "cost_price": "10.00",
            "track_stock": True,
            "min_stock": "1",
            "max_stock": "100",
            "location": None,
            "notes": None,
        },
    )
    product_id = int(product["id"])
    category_id = int(cat_alpha["id"])
    _open_products_list(page, api, name="Deactivate Widget")

    # Collect every /web/products GET from the moment the filter goes active, so
    # the flow's requests are the filtered ones and any unfiltered one fails.
    product_list_gets: list[str] = []

    def _record_list_get(request) -> None:
        parsed = urlparse(request.url)
        if parsed.path == _PRODUCTS_LIST and request.method == "GET":
            product_list_gets.append(request.url)

    page.on("request", _record_list_get)
    with page.expect_response(_response_for(_PRODUCTS_LIST)):
        page.locator("#filter-category").select_option(str(category_id))
    expect(page.locator(f"#{_PRODUCTS_INNER}")).to_contain_text("Deactivate Widget")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).not_to_contain_text(
        "Deactivate Other Widget"
    )

    # Deactivate the product from its drawer; the drawer closes on success. The
    # POST answer is what the browser swaps into #product-list, so its body must
    # already be the filtered fragment — the trigger re-fetch cannot be relied on
    # to mask an unfiltered render, and a settled-DOM check alone cannot tell the
    # swapped fragment from the re-fetch that follows it.
    _open_product_drawer(page, product_id)
    deactivate_form = page.locator(
        '#product-detail-inner form[hx-post="/web/products/deactivate"]'
    )
    with page.expect_response(_response_for("/web/products/deactivate", "POST")) as deactivated:
        deactivate_form.get_by_role("button", name="Deactivate").click()
    swapped = deactivated.value.text()
    assert "Deactivate Widget" in swapped, f"swapped answer must hold the row: {swapped:.400}"
    assert (
        "Deactivate Other Widget" not in swapped
    ), f"swapped answer must not hold the other category's row: {swapped:.400}"

    expect(page.locator("#product-drawer")).not_to_be_visible()
    expect(page.locator("#product-drawer-body")).to_have_text("")

    # The filtered list must still hold exactly the (now inactive) row.
    expect(page.locator(f"#{_PRODUCTS_INNER}")).to_contain_text("Deactivate Widget")
    expect(page.locator(f"#{_PRODUCTS_INNER}")).not_to_contain_text(
        "Deactivate Other Widget"
    )
    expect(page.locator(f"#{_PRODUCTS_INNER} > div[id^='product-']")).to_have_count(1)

    # Every /web/products GET the flow issued carried the active filter; an
    # unfiltered one means some caller rendered (or re-fetched) the catalogue.
    assert product_list_gets, "the flow must issue /web/products GETs"
    unfiltered = [
        url for url in product_list_gets if f"category_id={category_id}" not in url
    ]
    assert not unfiltered, f"unfiltered /web/products GETs during the flow: {unfiltered}"


def test_recording_a_supplier_cost_and_setting_preferred_from_the_drawer(
    page: Page, api: ApiClient
) -> None:
    """The drawer records a supplier cost and can mark its supplier preferred.

    Two suppliers are seeded so the preferred marker is unambiguous: only the row
    whose Set preferred was clicked may carry the badge, and the other supplier's
    row must keep its button. The cost must list with the supplier's name and the
    recorded amount, not a bare id or the product's fallback cost.
    """
    product = create_product(
        api,
        sku="COST-SKU-06",
        name="Cost Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    supplier_a = create_supplier(api, "Cost Supplier Alpha")
    supplier_b = create_supplier(api, "Cost Supplier Beta")
    record_supplier_cost(api, product_id, supplier_b, cost="8.00")

    _open_products_list(page, api, name="Cost Widget")
    _open_product_drawer(page, product_id)

    # The seeded cost renders from the product side before anything is recorded.
    rows = page.locator("#product-detail-inner div.mb-2.justify-between")
    expect(rows).to_have_count(1)
    expect(rows.filter(has_text="Cost Supplier Beta")).to_contain_text("8.00 USD")

    # Record a cost for the other supplier through the drawer's own form. The
    # drawer stays OPEN by design (so the recorded value is visible) — only the
    # Save product form closes it.
    cost_form = page.locator('#product-detail-inner form[hx-post="/web/product-costs"]')
    cost_form.locator('select[name="supplier_id"]').select_option(str(supplier_a))
    cost_form.locator('input[name="cost"]').fill("9.75")
    with page.expect_response(_response_for("/web/product-costs", "POST")):
        cost_form.get_by_role("button", name="Record cost").click()

    # The drawer was swapped in place and must still be open: both cost rows now
    # list with names and amounts.
    expect(page.locator("#product-drawer")).to_be_visible()
    rows = page.locator("#product-detail-inner div.mb-2.justify-between")
    expect(rows).to_have_count(2)
    expect(rows.filter(has_text="Cost Supplier Alpha")).to_contain_text("9.75 USD")

    # Mark the freshly recorded supplier preferred; the swap re-renders the rows.
    preferred_form_a = rows.filter(
        has_text="Cost Supplier Alpha"
    ).locator('form[hx-post="/web/product-costs/preferred"]')
    with page.expect_response(_response_for("/web/product-costs/preferred", "POST")):
        preferred_form_a.get_by_role("button", name="Set preferred").click()

    rows = page.locator("#product-detail-inner div.mb-2.justify-between")
    expect(rows).to_have_count(2)
    row_a = rows.filter(has_text="Cost Supplier Alpha")
    expect(row_a.get_by_text("Preferred", exact=True)).to_have_count(1)
    # The other supplier keeps its button and must not inherit the marker.
    row_b = rows.filter(has_text="Cost Supplier Beta")
    expect(row_b.get_by_text("Preferred", exact=True)).to_have_count(0)
    expect(row_b.locator('form[hx-post="/web/product-costs/preferred"]')).to_have_count(1)


# ---------------------------------------------------------------------------
# Stale-cost badge (cost-freshness F2-T1)
# ---------------------------------------------------------------------------


def _open_drawer_for_cost_scenario(
    page: Page, api: ApiClient, product: dict, name: str
):
    """Open the products page and the product's drawer, and return the drawer body.

    Shared by the stale-cost badge tests, which all seed a product, optionally
    record supplier costs, and then need the swapped detail fragment.
    """
    product_id = int(product["id"])
    _open_products_list(page, api, name=name)
    _open_product_drawer(page, product_id)
    drawer = page.locator("#product-detail-inner")
    expect(page.locator("#product-drawer")).to_be_visible()
    return drawer


def test_drawer_shows_stale_cost_badge_with_both_values_when_costs_disagree(
    page: Page, api: ApiClient
) -> None:
    """A stored cost that disagrees with the supplier reference shows both values.

    This is the browser's proof of the one thing the route-level tests cannot
    see: the operator actually SEES the disagreement without doing arithmetic.
    If the badge stopped carrying the values, an operator would read "stale
    cost" and still have to guess which side moved — so both the reference and
    the stored amount must render, not just the label.
    """
    product = create_product(
        api,
        sku="STALE-SKU-09",
        name="Stale Widget",
        cost_price="5.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    supplier_id = create_supplier(api, "Stale Supplier One")
    record_supplier_cost(api, int(product["id"]), supplier_id, cost="12.50")

    drawer = _open_drawer_for_cost_scenario(page, api, product, "Stale Widget")

    badge = drawer.get_by_text("Stale cost", exact=True)
    expect(badge).to_be_visible()
    # Both values in the badge's own sentence, as one exact text node: a
    # supplier row repeating an amount cannot satisfy it, and the bullet joins
    # them so either half dropping silently breaks the match.
    values = drawer.get_by_text("Reference 12.50 USD • Stored 5.00 USD", exact=True)
    expect(values).to_be_visible()
    expect(values).to_have_count(1)


def test_drawer_hides_stale_cost_badge_when_reference_equals_stored(
    page: Page, api: ApiClient
) -> None:
    """A supplier cost equal to the stored cost must not flag the column stale.

    The failure this guards is the disagreement gate collapsing to a subset
    (e.g. an always-true comparison): the badge would then render on every
    product with supplier rows, crying wolf and destroying the badge's value as
    a signal. Equal costs are the normal, fresh state and must render nothing.
    """
    product = create_product(
        api,
        sku="FRESH-SKU-10",
        name="Fresh Widget",
        cost_price="8.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    supplier_id = create_supplier(api, "Fresh Supplier Two")
    record_supplier_cost(api, int(product["id"]), supplier_id, cost="8.00")

    drawer = _open_drawer_for_cost_scenario(page, api, product, "Fresh Widget")

    # The badge's own label is the anchor: `Stale cost` appears nowhere else in
    # the templates (grep over the tree finds it only in the product detail
    # partial), so a page-level absence is discriminating and would catch the
    # badge rendering even outside the drawer body.
    expect(page.get_by_text("Stale cost", exact=True)).to_have_count(0)
    expect(drawer.get_by_text("reference 8.00 USD")).to_have_count(0)


def test_drawer_hides_stale_cost_badge_when_the_product_has_no_supplier_rows(
    page: Page, api: ApiClient
) -> None:
    """Without supplier rows the column IS the truth, so nothing can be stale.

    The comparison must not fall through to some implicit reference (the
    product's own cost, an empty cheapest pick): that would flag every
    supplier-less product, the exact opposite of the fallback the empty state
    message explains to the operator. No badge may render.
    """
    product = create_product(
        api,
        sku="NOSUP-SKU-11",
        name="Supplierless Widget",
        cost_price="5.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )

    drawer = _open_drawer_for_cost_scenario(
        page, api, product, "Supplierless Widget"
    )

    expect(page.get_by_text("Stale cost", exact=True)).to_have_count(0)
    # The card still tells the operator why there is no comparison to make.
    expect(drawer).to_contain_text("No supplier costs yet")


def test_drawer_hides_stale_cost_badge_when_the_stored_cost_is_zero(
    page: Page, api: ApiClient
) -> None:
    """A zero stored cost means "no cost recorded yet", never a disagreement.

    ``products.cost_price`` is ``NOT NULL DEFAULT '0'``, so zero is the column's
    empty state, not a value that can disagree with the supplier reference. A
    badge here would read as "your 7.25 USD supplier cost contradicts your (non)
    cost" — noise for a product that simply has not had its cost entered.
    """
    product = create_product(
        api,
        sku="ZERO-SKU-12",
        name="Zero Cost Widget",
        cost_price="0.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    supplier_id = create_supplier(api, "Zero Cost Supplier")
    record_supplier_cost(api, int(product["id"]), supplier_id, cost="7.25")

    drawer = _open_drawer_for_cost_scenario(page, api, product, "Zero Cost Widget")

    expect(page.get_by_text("Stale cost", exact=True)).to_have_count(0)
    expect(drawer.get_by_text("reference 7.25 USD")).to_have_count(0)


def test_recording_a_stock_movement_from_the_drawer_updates_both_surfaces(
    page: Page, api: ApiClient
) -> None:
    """The drawer's movement form updates the derived stock in drawer and list.

    The browser proves the wiring: recording an ``In`` from the product's own
    drawer must swap the fragment with the new derived stock and refresh the list
    row behind it via ``movement-created``. The arithmetic itself is the Rust
    suite's; what would break here is a movement that lands in neither surface.
    """
    product = create_product(
        api,
        sku="MOVE-SKU-07",
        name="Movement Widget",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    _open_products_list(page, api, name="Movement Widget")
    _open_product_drawer(page, product_id)
    expect(page.locator("#product-detail-inner")).to_contain_text("Stock 10")

    movement_form = page.locator(
        '#product-detail-inner form[hx-post="/web/stock-movements"]'
    )
    movement_form.locator('select[name="type"]').select_option("In")
    movement_form.locator('input[name="qty"]').fill("5")
    with page.expect_response(_response_for("/web/stock-movements", "POST")):
        with page.expect_response(_response_for(_PRODUCTS_LIST)):
            movement_form.get_by_role("button", name="Record movement").click()

    # The drawer was swapped with the fresh derived stock and must still be
    # open: only the Save product form closes it.
    expect(page.locator("#product-drawer")).to_be_visible()
    drawer = page.locator("#product-detail-inner")
    expect(drawer).to_contain_text("Stock 15")
    expect(drawer).not_to_contain_text("Stock 10")

    # The list row behind the drawer was refreshed by the trigger too.
    row = page.locator(f"#product-{product_id}")
    expect(row).to_contain_text("Stock 15")
    expect(row).not_to_contain_text("Stock 10")


# ---------------------------------------------------------------------------
# The server-computed price ladder (product price ladder U2)
# ---------------------------------------------------------------------------

_LADDER_PATH = "/web/product-price-ladder"


def _ladder(page: Page):
    """The price ladder island inside the open drawer."""
    return page.locator("#product-price-ladder")


def _ladder_amounts(page: Page) -> dict[str, str]:
    """Every money figure the ladder renders, keyed by the step that owns it.

    Read from the DOM rather than asserted as prose: the point of this test is
    that the FIGURES move, and a substring assertion over the whole island
    cannot tell a new net price from a new tax line.
    """
    rows = _ladder(page).locator("tr")
    amounts: dict[str, str] = {}
    for index in range(rows.count()):
        cells = rows.nth(index).locator("td")
        if cells.count() < 4:
            continue  # the refusal row spans the table; it publishes no figure
        amounts[cells.nth(0).inner_text().strip()] = cells.nth(3).inner_text().strip()
    return amounts


def test_typing_a_cost_in_the_drawer_moves_the_server_computed_ladder(
    page: Page, api: ApiClient
) -> None:
    """The ladder must follow the form in a REAL browser, before any save.

    This is the test the Rust suite could not be: the ladder's refresh rides
    ``hx-include="closest form"``, so the request the browser sends is the whole
    edit form, with the product under the form's own ``id`` key. A Rust test
    that hand-builds its own query can never catch a mismatch between the form's
    field names and the endpoint's, and this one did exactly that: the endpoint
    demanded a ``product_id`` the browser does not send, answered 400 to every
    real refresh, and the whole suite stayed green.

    So the assertions are about what the browser got back:

    * typing a new cost must move the derived net price AND the tax-inclusive
      price, because both are computed from the net;
    * the tax amounts must move with it, since a tax is a share of the net;
    * nothing may be saved: the product's stored net price, read back through
      the API, is still the price the drawer was opened with.

    A markup product is the sharp case: its net price is DERIVED, so a cost
    change has to move a figure the browser never computes.
    """
    product = create_product(
        api,
        sku="LADDER-SKU-20",
        name="Ladder Widget",
        sale_price="20.00",
        cost_price="10.00",
        markup_pct="100",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    tax = api.post_json(
        "/api/taxes",
        {"code": "IVA21", "name": "IVA 21%", "rate": "21", "is_active": True},
    )
    api.post_json(f"/api/products/{product_id}/taxes", {"tax_id": int(tax["id"])})

    _open_products_list(page, api, name="Ladder Widget")
    _open_product_drawer(page, product_id)
    expect(_ladder(page)).to_be_visible()

    # The drawer arrives showing the stored state: cost 10 with a 100% markup
    # derives 20.00, and 21% of that is 4.20, so 24.20 with tax.
    before = _ladder_amounts(page)
    assert before["Cost price"] == "10.00 USD", before
    assert before["Net price"] == "20.00 USD", before
    assert before["Price with tax"] == "24.20 USD", before

    form = _edit_form(page)
    form.locator('input[name="cost_price"]').fill("25.00")
    # The ladder is refreshed BY THE SERVER: wait for its own response, which is
    # the only thing that can move these figures.
    with page.expect_response(_response_for(_LADDER_PATH)):
        form.locator('input[name="cost_price"]').blur()

    # 25 * (1 + 100/100) = 50.00 net; 21% of 50.00 is 10.50; 60.50 with tax.
    after = _ladder_amounts(page)
    assert after["Cost price"] == "25.00 USD", after
    assert after["Net price"] == "50.00 USD", after
    assert after["IVA21 — IVA 21%"] == "10.50 USD", after
    assert after["Tax total"] == "10.50 USD", after
    assert after["Price with tax"] == "60.50 USD", after

    # And the preview changed NOTHING: the stored net price is still the one the
    # drawer was opened with, which is the whole point of a preview.
    stored = api.get_json(f"/api/products/{product_id}")
    assert stored["sale_price"] == "20.00", stored
    assert stored["cost_price"] == "10.00", stored


def test_the_ladder_says_a_refused_price_instead_of_showing_one(
    page: Page, api: ApiClient
) -> None:
    """A markup with no cost is a save refusal, and the ladder must say so.

    Cost 0 with a markup derives nothing, so the save path refuses the request.
    The ladder has to carry that refusal and publish no money: an operator who
    sees a tax total here would price a document with a number the server will
    never accept.
    """
    product = create_product(
        api,
        sku="LADDER-SKU-21",
        name="Refusal Widget",
        sale_price="20.00",
        cost_price="10.00",
        markup_pct="100",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    _open_products_list(page, api, name="Refusal Widget")
    _open_product_drawer(page, product_id)

    form = _edit_form(page)
    form.locator('input[name="cost_price"]').fill("0")
    with page.expect_response(_response_for(_LADDER_PATH)):
        form.locator('input[name="cost_price"]').blur()

    ladder = _ladder(page)
    expect(ladder.locator("[data-product-ladder-net-refused]")).to_be_visible()
    expect(ladder.locator("[data-product-ladder-net-refused]")).to_contain_text(
        "cost_price must be > 0 when markup_pct is set"
    )
    expect(ladder).not_to_contain_text("Price with tax")
    expect(ladder).not_to_contain_text("Tax total")

    # An unreadable field is the other half: the ladder stops previewing and
    # says the figures are the last saved ones, rather than reading "abc" as a
    # zero.
    form.locator('input[name="cost_price"]').fill("abc")
    with page.expect_response(_response_for(_LADDER_PATH)):
        form.locator('input[name="cost_price"]').blur()
    expect(ladder.locator("[data-product-ladder-unreadable]")).to_be_visible()
    fallback = _ladder_amounts(page)
    assert fallback["Cost price"] == "10.00 USD", fallback
    assert fallback["Net price"] == "20.00 USD", fallback


def test_switching_the_kind_in_the_drawer_moves_the_ladder_threshold(
    page: Page, api: ApiClient
) -> None:
    """The price rule branches on the kind, so the kind select must move the ladder.

    A Service may cost exactly 0.00; a Product may not. A service created at
    0.00 is therefore a legal state that the ladder publishes — and switching it
    to Product in the form must move the ladder onto the product threshold,
    which refuses that price. If the ladder bound the STORED kind it would keep
    publishing 0.00 while a save refuses, which is the defect this covers.

    Asserted through the real select, because the alternative — reading the
    ``hx-get`` off the markup — passes just as happily against a dead endpoint.
    """
    # A Service cannot track stock, so this one is seeded through the API
    # directly rather than through the tracked-product helper.
    service = api.post_json(
        "/api/products",
        {
            "sku": "LADDER-SVC-22",
            "name": "Free Service",
            "kind": "Service",
            "category_id": None,
            "unit": "un",
            "sale_price": "0.00",
            "cost_price": "5.00",
            "track_stock": False,
            "min_stock": None,
            "max_stock": None,
            "location": None,
            "notes": None,
        },
    )
    product_id = int(service["id"])
    tax = api.post_json(
        "/api/taxes",
        {"code": "IVA21", "name": "IVA 21%", "rate": "21", "is_active": True},
    )
    api.post_json(f"/api/products/{product_id}/taxes", {"tax_id": int(tax["id"])})

    _open_products_list(page, api, name="Free Service")
    _open_product_drawer(page, product_id)
    ladder = _ladder(page)

    # As stored: a free service is a price the server accepts.
    expect(ladder.locator("[data-product-ladder-net-refused]")).to_have_count(0)
    amounts = _ladder_amounts(page)
    assert amounts["Net price"] == "0.00 USD", amounts
    # The tax on a zero net is a zero: pinned to cents, and a zero keeps the
    # scale the arithmetic produced, so the server renders "0 USD".
    assert amounts["IVA21 — IVA 21%"] == "0 USD", amounts

    # Switched to Product in the form: the product rule refuses 0.00, so the
    # ladder must carry the refusal and withdraw every derived figure.
    form = _edit_form(page)
    with page.expect_response(_response_for(_LADDER_PATH)):
        form.locator('select[name="kind"]').select_option("Product")

    expect(ladder.locator("[data-product-ladder-net-refused]")).to_be_visible()
    expect(ladder.locator("[data-product-ladder-net-refused]")).to_contain_text(
        "sale_price must be > 0 for products"
    )
    expect(ladder).not_to_contain_text("Price with tax")
    expect(ladder).not_to_contain_text("Tax total")

    # And the stored kind is still a service: a preview changes nothing.
    stored = api.get_json(f"/api/products/{product_id}")
    assert stored["kind"] == "Service", stored
    assert stored["sale_price"] == "0.00", stored


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
    """Write full-page screenshots of the redesigned products states for a human.

    Skipped by default, like the harness artifact probe. It seeds a tracked
    product with stock and a supplier with a recorded cost so the drawer shows
    real content, then walks the states and writes one PNG per state under
    ``e2e/.artifacts/design/`` (git-ignored), numbering after the parties probe.
    """
    product = create_product(
        api,
        sku="SHOT-SKU-08",
        name="Screenshot Widget",
        sale_price="25.00",
        cost_price="10.00",
        stock="10",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])
    supplier_id = create_supplier(
        api,
        "Distribuidora Sur",
        phone="11 5555-5555",
        notes="Entregas los martes",
    )
    record_supplier_cost(api, product_id, supplier_id, cost="9.50")

    directory = ARTIFACTS_ROOT / "design"
    directory.mkdir(parents=True, exist_ok=True)
    written: list[str] = []

    def shot(name: str) -> None:
        path = directory / name
        page.screenshot(path=str(path), full_page=True)
        written.append(str(path.resolve()))

    # 1. Products list
    _open_products_list(page, api, name="Screenshot Widget")
    shot("07-products-list.png")

    # 2. New category modal open
    page.get_by_role("button", name="New category").click()
    expect(page.locator("#new-category-dialog")).to_be_visible()
    shot("08-products-category-modal.png")
    page.locator('#new-category-dialog button[type="button"]').first.click()
    expect(page.locator("#new-category-dialog")).not_to_be_visible()

    # 3. New product modal open
    page.get_by_role("button", name="New product").click()
    expect(page.locator("#new-product-dialog")).to_be_visible()
    shot("09-products-product-modal.png")
    page.locator('#new-product-dialog button[type="button"]').first.click()
    expect(page.locator("#new-product-dialog")).not_to_be_visible()

    # 4. Drawer open: edit form prefilled and the supplier-costs card with the
    #    recorded row.
    _open_product_drawer(page, product_id)
    expect(page.locator("#product-drawer")).to_be_visible()
    expect(page.locator("#product-detail-inner")).to_contain_text("Screenshot Widget")
    expect(page.locator("#product-detail-inner")).to_contain_text("Distribuidora Sur")
    shot("10-products-drawer.png")

    # 5. Drawer after recording a movement through the form, so the movement card
    #    is shown with a real interaction behind it.
    movement_form = page.locator(
        '#product-detail-inner form[hx-post="/web/stock-movements"]'
    )
    movement_form.locator('select[name="type"]').select_option("In")
    movement_form.locator('input[name="qty"]').fill("5")
    with page.expect_response(_response_for("/web/stock-movements", "POST")):
        movement_form.get_by_role("button", name="Record movement").click()
    expect(page.locator("#product-detail-inner")).to_contain_text("Stock 15")
    # The drawer body scrolls internally (the slide-over is fixed), so bring the
    # movement card into view before capturing it.
    page.locator("#product-drawer-body").evaluate("el => { el.scrollTop = el.scrollHeight; }")
    shot("11-products-drawer-movement.png")

    for path in written:
        print(f"screenshot: {path}")
