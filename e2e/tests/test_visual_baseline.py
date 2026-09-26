"""The visual-neutrality net for the component-token refactor.

`ui-component-tokens` moves styling out of `@layer base` and into component
classes. The whole point is that **no screen changes**, and that claim cannot be
checked by reading a diff: the classes are exactly what is supposed to change,
so a class diff proves nothing about what the operator sees.

So this records what the browser actually computes. For a fixed set of pages it
walks the DOM and, for every element, stores a fingerprint of its computed
style. The result is committed as a golden file and compared on every run.

Two deliberate choices:

- **Elements are keyed by DOM path, not by class.** The classes are the thing
  being rewritten; keying by them would make every entry look changed and the
  comparison useless. A path key says "the same element, in the same place".
- **The baseline is captured from the pre-refactor tree.** One captured
  afterwards would record whatever the refactor produced and prove nothing, so
  regeneration is behind an explicit environment variable that CI never sets.

Regenerate deliberately, and only when a change to the interface is intended:

    ROYA_VISUAL_BASELINE=write scripts/e2e.sh -k visual_baseline

The baseline holds states as well as pages. The notice dismiss buttons — the
controls that lean on the `@layer base` `a`/`button` rules — render only in
states the shared session never sees: the failure re-renders of the login and
password forms, the full-page permission refusal, and the three notice boxes
(base.html's client-side builder, the merge-scan server box and the
create-under-filter server box). Each state has its own name below; T2c
recorded the gap this closes: with the base rules removed, a pages-only net
passed while those five buttons lost their background, label colour, weight
and cursor.

**Two Settings pages were missing for the same reason and are now here.** The
`tailwind-stylesheet-rebuild` work unit rebuilt fifteen utility rules the
committed stylesheet lacked, and every one of them lives on `/settings` or
`/setup` — pages this net did not visit. A rebuild that the net cannot see is
a rebuild that was never proven, so the two Settings pages are now captured.

**`/setup` cannot be a capture, and saying so is part of the fix.** The
first-run wizard is one-time: this harness completes it before any test body
runs, and the server then answers `GET /setup` with a redirect to `/login`, so
the wizard is unreachable here (measured — see
`test_setup_wizard_applies_its_utility_classes`, which puts the server back in
a fresh-install state and asserts the wizard's classes on a real render). That
test is a computed-style assertion, not a snapshot: it covers the five classes
`/setup` owns and says nothing about the rest of that page, which stays
uncovered by this net.

**And a capture is not the same as an assertion — the limit is measured, not
assumed.** Thirteen of the fifteen rebuilt classes set a property this net
deliberately does not record (`width`, `max-width`, `min-height`,
`grid-template-columns`, `justify-content`, `align-self`, `align-items`,
`white-space`, and the two vertical margins); only `rounded-xl` and `pt-5`
declare one it does. Their elements are now fingerprinted, so any colour,
padding, radius, display, gap or opacity on them is covered, but a dropped
`md:grid-cols-2` would not fail here. The two tests at the end of this module
close that second gap by asserting the computed value each class is responsible
for, and every one of the fifteen is asserted by one of the two mechanisms.
One of them, `w-auto`, is only observable *below* the `md` breakpoint, which is
why the variant readings in this module exist and why every one of them is
taken at a second, narrower viewport as well.
"""

from __future__ import annotations

import json
import os
import urllib.parse
from datetime import date, timedelta
from pathlib import Path
from typing import Any

from playwright.sync_api import Page, expect

from conftest import TEST_ADMIN_USERNAME
from helpers import (
    ApiClient,
    HarnessData,
    account_method_id,
    add_purchase_line,
    confirm_purchase,
    create_product,
    create_purchase_draft,
    create_supplier,
    fund_account,
    reopen_first_run_setup_in_database,
    seed_harness_data,
    e2e_copy,
)
from test_identity import (
    CHANGED_PASSWORD,
    INITIAL_PASSWORD,
    _assign_role_through_the_screen,
    _change_confined_password,
    _create_user_through_the_screen,
    _log_in_through_the_form,
    _response_for,
)

BASELINE = Path(__file__).resolve().parent.parent / "visual-baseline.json"

# A fixed viewport so widths and heights are reproducible. The suite runs
# headless at 1280x720 by default; the desktop layout is what this protects.
VIEWPORT = {"width": 1440, "height": 900}

# Narrower than Tailwind's `sm` (40rem) and `md` (48rem) breakpoints, so a
# variant rule is provably *off* here. The tests at the end of this module read
# every class at both widths: a value that is the same either way could be
# something else's doing, and a variant rule that is not off at 600px is not a
# variant rule.
NARROW = {"width": 600, "height": 900}

# What a person actually sees. Layout-affecting properties are included on
# purpose: a refactor that drops a `gap` or a `padding` changes the screen even
# though no colour moved.
#
# **`width` and `height` are deliberately absent**, and the reason is the first
# thing CI taught this net. They were in the first version, and CI failed on a
# refactor that was visually correct: every one of the 88 reported differences
# was a width or a height - `95.3438px -> 110.656px`, `43px -> 42px` - and not a
# single colour, border, padding, font, display, gap, opacity or radius moved.
# Those two properties are text metrics: they follow the rendered font, and the
# runner's fonts are not this machine's. A net that fails for the environment is
# a net people learn to re-run until it passes, which is worse than no net at
# all. So it asserts what the classes control and leaves what the font controls
# to the behavioural suite.
#
# **`margin-top` and `margin-bottom` are out for the same reason**, and CI taught
# this one too, a round later: an auto-centred `<dialog>` resolves its vertical
# margin from its own height, which follows its content, which follows the font.
# The runner reported `325px -> 326.5px` on a refactor that was visually correct.
#
# So the rule is one rule: **the net asserts what the classes control directly,
# and nothing that resolves from content or font.** That is why `width`,
# `height` and the vertical margins are absent while `padding`, `gap`, `display`,
# `border-*-width`, the colours and the typography are all present.
#
# The cost is stated rather than hidden: a dropped `w-full`, `w-20` or `mb-4`
# would no longer be caught here. Everything above still is, and a layout
# regression that matters is a behavioural test's job.
#
# **Measured, not assumed: of the fifteen classes
# `tailwind-stylesheet-rebuild` added, this property set can only ever see two.**
# Counting the property each rule declares against the list below:
# `rounded-xl` declares `border-radius`, which `border-top-left-radius` records,
# and `pt-5` declares `padding-top`, which is recorded. The other thirteen
# declare `width`, `max-width`, `min-height`, `grid-template-columns` (four of
# them), `justify-content`, `align-self`, `align-items`, `white-space` or a
# vertical margin — none of which is in this list, and the last two of which are
# excluded on purpose. So visiting a page is necessary for a class to be
# covered and not sufficient: the class's element has to be fingerprinted *and*
# its effect has to be one of the properties recorded. Of the two that qualify,
# only `rounded-xl` is on a page this net visits — `pt-5` lives on `/setup`,
# which it cannot. The tests at the end of this module cover the other
# fourteen, and `rounded-xl` too, so all fifteen are asserted.
STYLE_PROPERTIES = [
    "color",
    "background-color",
    "border-top-width",
    "border-top-style",
    "border-top-color",
    "border-top-left-radius",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "font-size",
    "font-weight",
    "letter-spacing",
    "text-transform",
    "text-decoration-line",
    # Class-determined like the rest: the .choice component tints its native
    # checkbox/radio with accent-color, and a property the net does not record
    # would leave that tint unasserted.
    "accent-color",
    "display",
    "gap",
    "opacity",
    "cursor",
]

# Walks the whole body and fingerprints every element. The path is built from
# tag names and child indices only, so it survives a class rewrite.
_WALK = """
(props) => {
  const out = {};
  const walk = (el, path) => {
    const cs = getComputedStyle(el);
    const fp = {};
    for (const p of props) fp[p] = cs.getPropertyValue(p);
    out[path] = fp;
    let i = 0;
    for (const child of el.children) {
      walk(child, path + "/" + child.tagName.toLowerCase() + "[" + i + "]");
      i++;
    }
  };
  walk(document.body, "body");
  return out;
}
"""


def _pages(data: HarnessData) -> list[tuple[str, str]]:
    """The screens the component set touches, with a stable name each."""
    return [
        ("dashboard", "/"),
        ("products", "/products"),
        ("purchases", "/purchases"),
        ("purchase-record", f"/purchases/{data.purchase_id}"),
        ("sales", "/sales"),
        ("sale-record", f"/sales/{data.sale_id}"),
        ("customers", "/customers"),
        ("suppliers", "/suppliers"),
        ("documents", "/documents"),
        ("users", "/users"),
        ("roles", "/roles"),
        # The account screen is the only place the method checkboxes render
        # (the dashboard's create form carries the same shape). T5 puts `.field`
        # on checkboxes and radios, so the screens where they live must be in
        # the net before that can be provable.
        ("account-detail", f"/accounts/{data.account_id}"),
        # The password form is reachable from the sidebar for every signed-in
        # operator; its dismiss button lives only in the failure re-render,
        # captured separately as `password-error` below.
        ("password", "/password"),
        # The two Settings pages, added when fifteen rebuilt utility rules all
        # turned out to live on `/settings` and `/setup` — pages this net did
        # not visit, which left the rebuild unproven on all of its own surface.
        #
        # Both tabs are the same page under two URLs, and both are captured:
        # the business tab owns the identity/presentation form and the locale
        # profiles, the Taxes tab owns the tax catalogue. The Taxes tab is
        # captured in its resting state, with no tax defined, because a
        # definition would also reach the Products drawer (its link-tax select
        # lists every definition) and so would move an existing capture — the
        # `whitespace-nowrap` class only renders on a tax *row*, so that one
        # class is covered by the computed-style test instead.
        ("settings", "/settings"),
        ("settings-taxes", "/settings?tab=taxes"),
    ]


# The states outside the shared session, one distinct name each. All are
# deterministic by construction: fixed copy, seeded data, no timestamps and no
# ids that change between runs (the server is throwaway and freshly seeded, so
# even the DOM paths that carry line ids repeat exactly).
_STATE_NAMES = [
    "login",
    "login-error",
    "password-error",
    "forbidden",
    "purchase-record-notice",
    "purchase-record-merge",
    "products-create-under-filter",
    # The purchase list's three payment states (added with T3): chip-warning
    # exists only on a confirmed credit purchase owed inside its due date and
    # the Overdue chip only past it, and the harness seed's purchase is a
    # Draft — a pages-only net renders neither colour. Each state rides its
    # own supplier so the list filter isolates the one row whose chip colour
    # it protects.
    "purchase-list-paid",
    "purchase-list-due",
    "purchase-list-overdue",
    # T5's form controls: the confirm dialog's payment-type radios and this
    # page's picker are not reachable through any resting page snapshot — the
    # dialog only exists once `openRecordDialog` opens it, and the picker's
    # no-match box only exists once a query has been answered empty. Each is
    # captured from the current tree, which renders correctly today.
    "purchase-record-confirm",
    "purchase-record-picker-nomatch",
]


def _fingerprint(page: Page) -> dict[str, Any]:
    return page.evaluate(_WALK, STYLE_PROPERTIES)


# The resting state is not the whole story: the `@layer base` rules this refactor
# removes also carry `a:hover { text-decoration: underline }` and
# `button:hover { opacity: .9 }`. Those live in a state no resting snapshot can
# see, and they are exactly what disappears when the base rules go. So the hover
# state of every anchor and button is captured too.
#
# Real hovers, not a synthetic event: CSS `:hover` needs a pointer position, and
# no element in this app triggers htmx on a mouse event, so hovering has no side
# effects (checked, not assumed).
#
# **Keyed by the element being hovered, and that element's own style is read.**
# The first version keyed by whichever `:hover` chain ended deepest, which is
# whichever element the pointer happened to land on - geometry, and therefore
# font metrics. CI failed on it twice: the same nav item recorded `a[3]` locally
# and `a[3]/span[1]` on the runner, because a label's width decided whether the
# anchor's centre fell on its span. A key the environment chooses is not a key.
# Hovering an element makes it `:hover` whether the pointer is over it or over a
# descendant, so reading the target itself is both correct and stable.
_PATH_OF = """
(e) => {
  const parts = [];
  let node = e;
  while (node && node !== document.body) {
    const parent = node.parentElement;
    parts.unshift(node.tagName.toLowerCase() + "[" +
      (parent ? Array.from(parent.children).indexOf(node) : 0) + "]");
    node = parent;
  }
  return "body/" + parts.join("/");
}
"""

_STYLE_OF = """
(e, props) => {
  const cs = getComputedStyle(e);
  const fp = {};
  for (const p of props) fp[p] = cs.getPropertyValue(p);
  return fp;
}
"""


def _hover_fingerprint(page: Page) -> tuple[dict[str, Any], list[str]]:
    """Hover every anchor and button and record what the browser computes.

    Returns the fingerprints plus the elements whose hover could not be
    performed, by path, so a change in that set is visible rather than silent.
    """
    out: dict[str, Any] = {}
    skipped: list[str] = []
    targets = page.locator("a, button")
    for i in range(targets.count()):
        el = targets.nth(i)
        path = el.evaluate(_PATH_OF)
        try:
            # `force` skips Playwright's stability wait, which costs ~340ms per
            # element because this app transitions colours and transforms. The
            # wait buys nothing here: a hover that lands on the wrong element
            # would make the run non-deterministic, and determinism is checked.
            el.hover(force=True, timeout=2000)
        except Exception:
            skipped.append(path)
            continue
        out[path] = el.evaluate(_STYLE_OF, STYLE_PROPERTIES)
    return out, skipped


def test_visual_baseline(
    page: Page,
    api: ApiClient,
    browser,
    browser_context_args: dict,
    live_server,
) -> None:
    """Every screen computes the same styles it did before the refactor."""
    data = seed_harness_data(api)
    page.set_viewport_size(VIEWPORT)

    current: dict[str, Any] = {}

    def capture(subject: Page, name: str) -> None:
        current[name] = _fingerprint(subject)
        hover, skipped = _hover_fingerprint(subject)
        current[name + ":hover"] = hover
        current[name + ":hover-skipped"] = skipped

    for name, path in _pages(data):
        page.goto(f"{api.base_url}{path}")
        page.wait_for_load_state("networkidle")
        capture(page, name)

    # The drawers are the richest component surfaces in the app and they are
    # fragments, not pages, so they are reached by clicking. Snapshot the open
    # state, which is the only state an operator ever styles.
    for name, path, row in (
        ("products-drawer", "/products", f"#product-{data.product_id} button"),
        ("purchases-drawer", "/purchases", f"#purchase-{data.purchase_id}"),
    ):
        page.goto(f"{api.base_url}{path}")
        page.locator(row).first.click()
        page.wait_for_timeout(400)
        current[name] = _fingerprint(page)

    # -- The purchase list's payment states ----------------------------------
    # The Rust guard `purchase_list_row_reads_identifier_supplier_money_with_
    # one_status_chip` seeds exactly the Paid/Due/Overdue triad; the same
    # recipe through the API helpers seeds it here. The harness account
    # already owns Cash (a second account cannot claim the method), so it is
    # funded and the confirm pays through it. Paid is a Cash confirm
    # (it posts the payment itself, so due lands at 0); Due is a confirmed
    # Credit with a due date 30 days out — a fixed date would honestly render
    # Overdue; Overdue is a confirmed Credit already past its due date. One
    # supplier per state so `?supplier=` filters the list down to the one row
    # whose chip colour the state exists to protect.
    state_cash = account_method_id(api, data.account_id, "Cash")
    fund_account(api, data.account_id, "1000")
    state_product = create_product(
        api,
        sku="NET-STATE-SKU",
        name="Net State Widget",
        sale_price="20.00",
        cost_price="6.00",
        stock="30",
        min_stock="1",
        max_stock="100",
    )
    state_specs = (
        ("purchase-list-paid", "Net Paid Supplier", None, "Cash"),
        (
            "purchase-list-due",
            "Net Due Supplier",
            (date.today() + timedelta(days=30)).isoformat(),
            "Credit",
        ),
        ("purchase-list-overdue", "Net Overdue Supplier", "2024-05-02", "Credit"),
    )
    for state_name, supplier_name, due_date, payment_type in state_specs:
        state_supplier = create_supplier(api, supplier_name)
        state_purchase = create_purchase_draft(
            api,
            state_supplier,
            payment_type=payment_type,
            due_date=due_date,
        )
        add_purchase_line(api, state_purchase, int(state_product["id"]), qty="1")
        confirm_purchase(
            api,
            state_purchase,
            method_id=state_cash if payment_type == "Cash" else None,
        )
        page.goto(
            f"{api.base_url}/purchases?supplier={urllib.parse.quote(supplier_name)}"
        )
        page.wait_for_load_state("networkidle")
        capture(page, state_name)

    # -- The notice states -----------------------------------------------------
    # The purchase record is the cheapest route into two of the three notice
    # copies: a successful data-action form raises base.html's client-side box
    # (the JS `notice()` builder), and a repeat scan of a product already on
    # the draft answers the server-rendered merge box. One page, two states.
    page.goto(f"{api.base_url}/purchases/{data.purchase_id}")
    page.wait_for_load_state("networkidle")
    add_line = page.locator('form[data-action="Add line"]')
    # The spare product is not on the draft yet, so this is an ordinary add:
    # the generic client-side notice is what it leaves behind.
    add_line.locator("#product-picker").fill("HARNESS-SPARE")
    # The island debounces a search behind every keystroke; let it finish so a
    # late response cannot repaint the re-rendered entry row mid-snapshot.
    page.wait_for_load_state("networkidle")
    with page.expect_response(
        _response_for(f"/web/purchases/{data.purchase_id}/lines", "POST")
    ):
        add_line.get_by_role("button", name="Add line").click()
    expect(page.locator("[data-notice='success']")).to_contain_text("Add line saved")
    page.wait_for_timeout(400)
    capture(page, "purchase-record-notice")

    # The seeded line's product at the same resolved cost: the server merges
    # instead of answering 400 and swaps the merge notice out of band.
    add_line.locator("#product-picker").fill(data.barcode)
    page.wait_for_load_state("networkidle")
    with page.expect_response(
        _response_for(f"/web/purchases/{data.purchase_id}/lines", "POST")
    ):
        add_line.get_by_role("button", name="Add line").click()
    expect(page.locator("[data-notice='success']")).to_contain_text("scanned again")
    page.wait_for_timeout(400)
    capture(page, "purchase-record-merge")

    # The confirm dialog is not a page: it exists only once the action bar's
    # button calls openRecordDialog, the way the drawers are reached by
    # clicking rather than by URL. Its payment-type radios, due-date input and
    # method select are form controls the net would otherwise never fingerprint.
    page.goto(f"{api.base_url}/purchases/{data.purchase_id}")
    page.wait_for_load_state("networkidle")
    page.locator("button[onclick*=\"openRecordDialog('confirm-purchase')\"]").click()
    page.wait_for_timeout(400)
    capture(page, "purchase-record-confirm")

    # The picker's no-match state: type a query nothing answers, let the
    # island's debounced search come back empty and snapshot the dashed
    # no-match box the island renders from JS (static/picker.js).
    page.goto(f"{api.base_url}/purchases/{data.purchase_id}")
    page.wait_for_load_state("networkidle")
    page.locator("#product-picker").fill("zzz-no-product-matches")
    expect(page.locator("#product-search-results")).to_contain_text(
        "No products match"
    )
    page.wait_for_timeout(400)
    capture(page, "purchase-record-picker-nomatch")

    # The third server-rendered copy: creating a product under an active
    # catalogue filter. The create form carries the filter in its body
    # (hx-include), so a non-empty search text is all the state it needs.
    page.goto(f"{api.base_url}/products")
    page.wait_for_load_state("networkidle")
    page.locator('input[name="q"]').fill("no-product-matches-this-filter")
    page.wait_for_load_state("networkidle")
    dialog = page.locator("#new-product-dialog")
    page.get_by_role("button", name="New product").click()
    dialog.locator('input[name="sku"]').fill("BASELINE-NOTICE")
    dialog.locator('input[name="name"]').fill("Filter Notice Product")
    dialog.locator('input[name="sale_price"]').fill("1.00")
    with page.expect_response(_response_for("/web/products", "POST")):
        dialog.get_by_role("button", name="Create product").click()
    expect(page.locator("[data-notice='success']")).to_contain_text(
        "Filter Notice Product created"
    )
    page.wait_for_timeout(400)
    capture(page, "products-create-under-filter")

    # The password form's dismiss button lives only in its failure re-render.
    # A wrong current password verifies before any write, so nothing changes.
    page.goto(f"{api.base_url}/password")
    page.get_by_label(e2e_copy("current_password")).fill("definitely-not-the-password")
    page.get_by_label(e2e_copy("new_password"), exact=True).fill(CHANGED_PASSWORD)
    page.get_by_label(e2e_copy("confirm_password")).fill(CHANGED_PASSWORD)
    page.get_by_role("button", name=e2e_copy("save_password")).click()
    expect(page.locator("[data-notice='error']")).to_be_visible()
    page.wait_for_timeout(400)
    capture(page, "password-error")

    # -- /login and /forbidden: states outside the shared session --------------
    # The `page` fixture is authenticated, so the gate states ride their own
    # context, the way tests/test_identity.py builds an anonymous visitor.
    anonymous_context = browser.new_context(**browser_context_args)
    anonymous = anonymous_context.new_page()
    anonymous.set_viewport_size(VIEWPORT)
    try:
        anonymous.goto(f"{api.base_url}/login")
        anonymous.wait_for_load_state("networkidle")
        capture(anonymous, "login")
        # The login page's dismiss button lives only in the failure re-render.
        anonymous.get_by_label(e2e_copy("username")).fill(TEST_ADMIN_USERNAME)
        anonymous.get_by_label(e2e_copy("password")).fill("definitely-not-the-password")
        anonymous.get_by_role("button", name=e2e_copy("sign_in")).click()
        expect(anonymous.locator("[data-notice='error']")).to_be_visible()
        anonymous.wait_for_timeout(400)
        capture(anonymous, "login-error")

        # /forbidden: a principal with no permissions at all, built through
        # the same real-screen route test_identity.py uses — a fresh role
        # (a new role holds no permissions, so no matrix edit is needed), a
        # screen-created user, the role assigned, the confined first login
        # and the password change. The landing on / is then refused for the
        # missing dashboard.read, and the full-page refusal card is the
        # state the net captures.
        page.goto(f"{api.base_url}/roles")
        page.get_by_role("button", name=e2e_copy("new_role")).click()
        role_dialog = page.locator("#new-role-dialog")
        role_dialog.locator('input[name="code"]').fill("sin_permisos")
        role_dialog.locator('input[name="name"]').fill("Sin permisos")
        role_dialog.locator('input[name="description"]').fill(
            "Cero permisos: lo siembra la red visual para /forbidden."
        )
        with page.expect_response(_response_for("/web/roles", "POST")):
            role_dialog.get_by_role("button", name=e2e_copy("create_role")).click()
        expect(page.locator("#role-list")).to_contain_text("sin_permisos")

        page.goto(f"{api.base_url}/users")
        _create_user_through_the_screen(
            page,
            username="sinpermisos1",
            display_name="Sin Permisos Uno",
            password=INITIAL_PASSWORD,
        )
        _assign_role_through_the_screen(
            page, username="sinpermisos1", role_name="Sin permisos"
        )

        _log_in_through_the_form(
            anonymous, live_server, "sinpermisos1", INITIAL_PASSWORD
        )
        expect(anonymous).to_have_url(f"{api.base_url}/password")
        _change_confined_password(
            anonymous, current=INITIAL_PASSWORD, new=CHANGED_PASSWORD
        )
        # The post-change landing is the refused dashboard: the full-page card.
        expect(anonymous.locator("[data-notice='error']")).to_contain_text(
            "dashboard.read"
        )
        capture(anonymous, "forbidden")
    finally:
        anonymous_context.close()

    if os.environ.get("ROYA_VISUAL_BASELINE") == "write":
        BASELINE.write_text(json.dumps(current, sort_keys=True, separators=(",", ":")) + "\n")
        print(f"\nwrote {BASELINE} ({BASELINE.stat().st_size} bytes)", flush=True)
        return

    assert BASELINE.exists(), (
        f"no baseline at {BASELINE}. Capture it from the tree whose appearance "
        "you want to freeze: ROYA_VISUAL_BASELINE=write scripts/e2e.sh -k "
        "visual_baseline. A baseline captured AFTER the change you are trying to "
        "detect records that change and proves nothing."
    )
    expected = json.loads(BASELINE.read_text())

    # Report the first differences in a shape a person can act on, rather than
    # dumping two dictionaries at each other.
    problems: list[str] = []
    names = [n for n, _ in _pages(data)] + [
        n + suffix for n, _ in _pages(data) for suffix in (":hover", ":hover-skipped")
    ] + [
        name + suffix
        for name in _STATE_NAMES
        for suffix in ("", ":hover", ":hover-skipped")
    ] + ["products-drawer", "products-drawer:hover", "products-drawer:hover-skipped",
         "purchases-drawer", "purchases-drawer:hover", "purchases-drawer:hover-skipped"]
    for name in names:
        want, got = expected.get(name, {}), current.get(name, {})
        if want == got:
            continue
        if name.endswith(":hover-skipped"):
            problems.append(f"{name}: {want!r} -> {got!r}")
            if len(problems) > 40:
                problems.append("... (stopping after 40 differences)")
                break
            continue
        for dom_path in sorted(set(want) | set(got)):
            if want.get(dom_path) == got.get(dom_path):
                continue
            if dom_path not in want:
                problems.append(f"{name}: element appeared at {dom_path}")
            elif dom_path not in got:
                problems.append(f"{name}: element gone from {dom_path}")
            else:
                for prop in STYLE_PROPERTIES:
                    a, b = want[dom_path].get(prop), got[dom_path].get(prop)
                    if a != b:
                        problems.append(
                            f"{name}: {dom_path}\n      {prop}: {a!r} -> {b!r}"
                        )
            if len(problems) > 40:
                problems.append("... (stopping after 40 differences)")
                break
        if len(problems) > 40:
            break

    assert not problems, (
        "the interface changed.\n"
        "  If that was not intended, the diff above is the defect.\n"
        "  If it WAS intended, regenerate deliberately and say so in the commit:\n"
        "      ROYA_VISUAL_BASELINE=write scripts/e2e.sh -k visual_baseline\n"
        "  A baseline regenerated to make a red test green is not evidence.\n\n  "
        + "\n  ".join(problems)
    )


# ---------------------------------------------------------------------------
# What the snapshots above cannot say
# ---------------------------------------------------------------------------
#
# Two gaps, one shape: a class the templates use is not *asserted* to be in
# force. The snapshots fix the first by visiting the page; the tests below fix
# the second by naming the value the class is responsible for, which is the
# only form in which a dropped rule is visible.
#
# They read the same `getComputedStyle` the snapshots read, and deliberately
# use the same property names, so "the net records this" and "the net can see
# this" stay one question with one answer.

_COMPUTED_FOR_CLASS = """
([className, prop]) => Array.from(
  document.querySelectorAll("." + CSS.escape(className))
).map((el) => getComputedStyle(el).getPropertyValue(prop))
"""


def _computed(page: Page, class_name: str, prop: str) -> list[str]:
    """What `prop` computes to on every element of this page carrying the class.

    `CSS.escape` is what makes `sm:grid-cols-[100px_1fr_120px_auto]` selectable at
    all; a hand-written selector for a variant or an arbitrary value is a
    different selector than the one the class names.

    An empty list means the class is on no element of this page, which is a
    different failure from a wrong value: the assertion below tells them apart
    instead of letting an absent class pass for a correct one.
    """
    return page.evaluate(_COMPUTED_FOR_CLASS, [class_name, prop])


def _assert_computes(page: Page, class_name: str, prop: str, expected: str) -> None:
    values = _computed(page, class_name, prop)
    assert values, (
        f"{class_name!r} is on no element of this page, so {prop} would be "
        "asserted for nothing"
    )
    distinct = sorted(set(values))
    assert distinct == [expected], (
        f"{class_name!r} computes {prop} as {distinct}, expected only "
        f"{expected!r} - the rule is not in force on every element that asks "
        "for it, or something else is winning"
    )


def _assert_grid_tracks(
    page: Page,
    class_name: str,
    expected_tracks: int,
    fixed_tracks: tuple[tuple[int, str], ...] = (),
) -> None:
    """Assert the track list a `grid-cols-*` class lays out.

    A grid class's value is a track list, so the two facts worth pinning are how
    many tracks exist and the sizes the class names literally. The `fr` and
    `auto` tracks resolve from the content and the viewport, so their pixel
    values are deliberately not asserted: a value that follows the content is
    the class of value this file already refuses to pin.
    """
    values = _computed(page, class_name, "grid-template-columns")
    assert values, f"{class_name!r} is on no element of this page"
    for value in values:
        tracks = value.split()
        assert len(tracks) == expected_tracks, (
            f"{class_name!r} laid out {len(tracks)} track(s) {tracks}, expected "
            f"{expected_tracks}"
        )
        for index, size in fixed_tracks:
            assert tracks[index] == size, (
                f"{class_name!r} track {index} is {tracks[index]!r}, expected "
                f"{size!r} - the arbitrary value in this class is not in force"
            )


def _assert_above_and_below_breakpoint(
    page: Page,
    class_name: str,
    prop: str,
    above: str,
    below: str | None,
) -> None:
    """Assert a class at `VIEWPORT` and at `NARROW`, in that order.

    `below=None` means the class is unconditional and must read the same at
    both widths; a value that only holds at one width would be a breakpoint
    assumption dressed up as a class assertion.

    Resizing is enough: `getComputedStyle` forces layout, and the only property
    under test is one a media query re-evaluates on its own. No sleep.
    """
    page.set_viewport_size(VIEWPORT)
    _assert_computes(page, class_name, prop, above)
    if below is None:
        return
    page.set_viewport_size(NARROW)
    _assert_computes(page, class_name, prop, below)


# `w-auto` is why this table reads at two widths at all, and an earlier draft of
# it got `w-auto` wrong by asserting only at the wide viewport. The rule is
# *inert at 1440, 900 and 768* and *observable at 767, 600 and 320*, and the
# cause is the grid it sits in (`templates/settings.html:105-112`): at `md` and
# above the label is in the `auto` track of `md:grid-cols-[1fr_auto]` and is
# content-sized, so the flex shrink algorithm lands on the checkbox's intrinsic
# 13px with or without the rule; below `md` the grid collapses to a single
# track, so `.field`'s `width:100%` becomes the flex base size and the checkbox
# fills the row unless `w-auto` stops it. The NARROW reading is therefore the
# one that carries the evidence, and the wide reading is kept as the control
# that shows the two agree exactly where the rule is not needed.
#
# **Recording `width` here is safe even though the snapshots refuse it.** The
# snapshots exclude `width` because it resolves from the rendered font and the
# runner's fonts are not this machine's. 13px is not a text measurement: it is
# the native checkbox's intrinsic size. Verified by changing the label's font
# with the layout otherwise untouched - a control text span inside the same
# label went 63.45px -> 179.78px -> 368.25px -> 613.75px across monospace 40px,
# serif 72px and a font that does not exist at 120px, while the checkbox held
# 13px in both its computed `width` and its `offsetWidth` at every one of them.
_SETTINGS_BUSINESS_TAB = [
    ("w-auto", "width", "13px", "13px"),
    # `--radius-xl` is .75rem, and the locale profile card declares no radius of
    # its own: without the rule this is 0.
    #
    # This entry and the baseline snapshot both assert `rounded-xl`, and the
    # overlap is deliberate rather than something to prune: `rounded-xl` is the
    # one class of the fifteen whose effect the snapshot asserts on its own,
    # because `border-top-left-radius` is in `STYLE_PROPERTIES`, and an auditor
    # reading this table is looking for every class the fifteen are. A note for
    # that auditor, because getting it wrong is easy and silent: **Tailwind
    # emits the `border-radius` SHORTHAND** —
    # `.rounded-xl { border-radius: var(--radius-xl); }` — and
    # `rule.style.getPropertyValue('border-top-left-radius')` on that rule is
    # the empty string, so a probe filtering the CSSOM on the longhand reports
    # **zero** rules and concludes the class is inert. `Array.from(rule.style)`
    # does list the four expanded longhands, and the *computed* longhand reads
    # 12px. Match the shorthand and the illusion goes away: deleting the rule
    # moves `border-top-left-radius` from 12px to 0px at 1440, 900, 600 and 320.
    ("rounded-xl", "border-top-left-radius", "12px", None),
    ("justify-end", "justify-content", "flex-end", None),
    ("md:items-end", "align-items", "flex-end", "normal"),
    # `mb-2` is .5rem, so the narrow value is the base class showing through.
    ("md:mb-0", "margin-bottom", "0px", "8px"),
]

_SETTINGS_TAXES_TAB = [
    ("self-end", "align-self", "flex-end", None),
    # `whitespace-nowrap` renders on a tax *row* only, so this needs a tax
    # defined; the baseline captures the Taxes tab empty, which is why the
    # class is asserted here rather than in a snapshot.
    ("whitespace-nowrap", "white-space", "nowrap", None),
]

# (class, tracks at VIEWPORT, tracks at NARROW, literal track sizes)
#
# Split per tab on purpose: a class belongs to the page that asks for it, and
# reading one on the other tab must fail loudly rather than be skipped. An
# earlier single list did fail that way, which is why the split exists.
_SETTINGS_BUSINESS_GRIDS = [
    ("md:grid-cols-2", 2, 1, ()),
    ("md:grid-cols-[1fr_auto]", 2, 1, ()),
]

_SETTINGS_TAXES_GRIDS = [
    ("sm:grid-cols-[100px_1fr_120px_auto]", 4, 1, ((0, "100px"), (2, "120px"))),
]

_SETTINGS_TABS = (
    ("/settings", _SETTINGS_BUSINESS_TAB, _SETTINGS_BUSINESS_GRIDS),
    ("/settings?tab=taxes", _SETTINGS_TAXES_TAB, _SETTINGS_TAXES_GRIDS),
)

# `max-w-2xl` consumes `--container-2xl`, 42rem, at the 16px root the base layer
# sets. `min-h-[70vh]` is read from the viewport height rather than hardcoded,
# so the assertion still says `70vh` after a viewport change.
_SETUP_WIZARD = [
    ("max-w-2xl", "max-width", "672px", None),
    ("min-h-[70vh]", "min-height", f"{round(VIEWPORT['height'] * 0.7)}px", None),
    ("mt-8", "margin-top", "32px", None),
    ("pt-5", "padding-top", "20px", None),
]

_SETUP_GRIDS = [("sm:grid-cols-2", 2, 1, ())]


def test_settings_pages_apply_their_utility_classes(page: Page, api: ApiClient) -> None:
    """The Settings classes the baseline cannot see are in force on a real render.

    The baseline now captures both tabs, so their elements are fingerprinted -
    but eight of the ten classes those pages own set a property
    `STYLE_PROPERTIES` does not record, and `whitespace-nowrap` renders only on
    a tax row the resting capture does not have. This asserts the computed value
    each class is responsible for, which is the only thing that fails when a
    rule is dropped - verified by removing each rule in turn, not assumed, and
    read at a second narrow viewport because one of them (`w-auto`) is
    indistinguishable from nothing at the wide one.
    """
    page.set_viewport_size(VIEWPORT)
    # One tax definition, through the same API the catalogue itself uses, so the
    # row that carries `whitespace-nowrap` exists. Its own rate and name are
    # fixed, and nothing here is compared against a snapshot.
    api.post_json(
        "/api/taxes",
        {"code": "NET-CLASS", "name": "Net class", "rate": "21.00", "is_active": True},
    )

    for path, classes, grids in _SETTINGS_TABS:
        page.goto(f"{api.base_url}{path}")
        page.wait_for_load_state("networkidle")
        for class_name, prop, above, below in classes:
            _assert_above_and_below_breakpoint(page, class_name, prop, above, below)
        for class_name, wide, narrow, fixed in grids:
            page.set_viewport_size(VIEWPORT)
            _assert_grid_tracks(page, class_name, wide, fixed)
            page.set_viewport_size(NARROW)
            _assert_grid_tracks(page, class_name, narrow)


def test_setup_wizard_applies_its_utility_classes(
    page: Page, api: ApiClient, live_server
) -> None:
    """The five classes the first-run wizard owns, on a real `/setup` render.

    `/setup` is one-time, so the harness - which completes first-run setup
    before any test body runs - can never reach it: the server answers the
    request with a redirect to `/login`, and a browser holding the session then
    lands on the dashboard having seen no wizard at all. That is why the wizard
    is not a baseline capture, and it is measured rather than assumed.

    So the server is put back in the state a brand-new installation is in, by
    removing the singleton configuration row from the throwaway database
    (`reopen_first_run_setup_in_database`). Everything else is real: the real
    route, the real template, the real committed stylesheet, read by the real
    browser. The development database is never involved.

    What this covers is exactly the five classes and the property each one is
    responsible for. The rest of the wizard's rendering is not covered by
    anything, and the task document says so.
    """
    reopen_first_run_setup_in_database(live_server.db_path)

    page.set_viewport_size(VIEWPORT)
    page.goto(f"{api.base_url}/setup")
    page.wait_for_load_state("networkidle")
    # The redirect is the failure this test works around, so it is asserted
    # rather than assumed: if the wizard ever becomes reachable through the
    # seeded session, this stops being the only way to see it.
    expect(page).to_have_url(f"{api.base_url}/setup")

    for class_name, prop, above, below in _SETUP_WIZARD:
        _assert_above_and_below_breakpoint(page, class_name, prop, above, below)
    for class_name, wide, narrow, fixed in _SETUP_GRIDS:
        page.set_viewport_size(VIEWPORT)
        _assert_grid_tracks(page, class_name, wide, fixed)
        page.set_viewport_size(NARROW)
        _assert_grid_tracks(page, class_name, narrow)
