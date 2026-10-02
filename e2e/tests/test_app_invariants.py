"""App-wide invariants: one net under every page, instead of one per department.

The four defects that reached `main` behind 1480 green Rust tests and 134 green
browser tests had one thing in common: **no test visited every page**. Coverage
was departmental, so a brand new screen started at zero, and that is the door
they came through.

- The return lists and credit-note lists rendered 200 with no console error and
  had no sidebar entry, so they were reachable only by typing the URL.
- The creation dialog posted an empty identifier and its only possible answer
  was a 422.
- That 422 showed the operator serde internals.
- The money chip stayed stale on four document records and read `Paid` over a
  document that had collected nothing.

This module does not describe any page. It derives the registered page routes
from the router registration itself, walks them, and asserts properties that
hold for *every* page a shop can open. A property that has to be written once
per screen is a property that will be forgotten on the next screen.

What it asserts:

1. No page logs a console error or throws an uncaught exception.
2. No page renders the text of an internal error (serde, panics, Rust type
   names, source paths) — matched by shape, not by listing the types.
3. No page carries a duplicate element `id`.
4. Every named form control is programmatically labelled.
5. A person can actually get there: every sidebar entry navigates to the route
   it declares, every registered page route is linked from somewhere in the
   shell, and every record page's dialog-opening action responds.

**What it deliberately does not do.** It never mutates: no submit of a form
that writes, no `hx-post`, no `hx-delete`. A net that changes the shop while it
measures it is a net nobody can trust.

**Excluded, and the exclusion is measured rather than promised.** `/login` and
`/setup` render outside the signed-in shell: the harness's browser context
carries a session, so `GET /login` and `GET /setup` answer a redirect to `/`.
They cannot be exercised from here — `tests/test_identity.py` and
`tests/test_visual_baseline.py` cover them from a server that is genuinely
fresh — so this module asserts the redirect and excludes them from the sweep
instead of pretending the dashboard is the wizard.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any, Iterator
from urllib.parse import urlsplit

from playwright.sync_api import Page

from helpers import (
    ApiClient,
    create_confirmed_credit_purchase,
    create_confirmed_credit_sale,
    create_customer,
    create_product,
    create_supplier,
    seed_harness_data,
)

# ---------------------------------------------------------------------------
# The registered page routes, read from the registration itself
# ---------------------------------------------------------------------------

ROUTES_DIR = Path(__file__).resolve().parents[2] / "src" / "routes"

# Every module that registers a browser route. `web.rs` is the dashboard
# module and is not named `*_web.rs`; the API routers (`*_api.rs`, `api.rs`)
# register JSON endpoints under `/api/`, which are not pages.
_ROUTE_SOURCES = [ROUTES_DIR / "web.rs", *sorted(ROUTES_DIR.glob("*_web.rs"))]

# One `.route("...", <methods>)` call. The non-greedy method group stops at the
# first `)` followed by a comma or a line end, which is how both the one-line
# `get(a).post(b)` shape and the multi-line `post(\n  handler,\n)` shape end.
_ROUTE_CALL = re.compile(r'\.route\(\s*"([^"]+)"\s*,\s*(.*?)\)\s*(?:,|\n)', re.S)
_HAS_GET = re.compile(r"\bget\(")

# `/web/...` serves htmx fragments, not pages: they are swapped into a region of
# a page that was already loaded, and three of them answer `400` on a bare GET
# because they require an hx request or a query. The page is the shell.
_FRAGMENT_PREFIX = "/web/"

# The two pages that render outside the signed-in shell. See the module
# docstring: the exclusion is asserted, not assumed.
_ANONYMOUS_PAGE_ROUTES = frozenset({"/login", "/setup"})

# The route template's first path segment names the entity its `{id}` refers to.
# A new route family with a parameter and no entry here fails loudly in
# `_instantiate` rather than being silently skipped, because a sweep that
# quietly drops the route it cannot build is a sweep that proves nothing.
_PARAM_OWNER = {
    "accounts": "account_id",
    "sales": "sale_id",
    "purchases": "purchase_id",
    "customers": "customer_id",
    "purchase-returns": "purchase_return_id",
    "customer-returns": "customer_return_id",
}


def registered_page_routes() -> list[str]:
    """Every GET page route the application registers, from its own source.

    The registry is the source of truth, not a list kept here: a screen added
    tomorrow is swept tomorrow, and a screen deleted fails this module instead
    of leaving a stale expectation. The two guards below make a broken
    extraction fail loudly instead of returning a short list that would make
    every sweep pass vacuously.
    """
    routes: list[str] = []
    for source in _ROUTE_SOURCES:
        text = source.read_text(encoding="utf-8")
        for match in _ROUTE_CALL.finditer(text):
            path, methods = match.group(1), match.group(2)
            if path.startswith(_FRAGMENT_PREFIX):
                continue
            if not _HAS_GET.search(methods):
                continue
            routes.append(path)

    assert routes, (
        "route extraction found no page route in "
        f"{[source.name for source in _ROUTE_SOURCES]}; the registration shape "
        "changed and this module is now blind"
    )
    assert "/" in routes and all(path.startswith("/") for path in routes), (
        f"route extraction produced something that is not a route: {routes}"
    )
    assert len(routes) >= 15, (
        f"route extraction found only {len(routes)} page routes ({routes}); "
        "that is too few to be the app, so the sweep would prove nothing"
    )
    return routes


def _instantiate(template: str, context: dict[str, Any]) -> str:
    """Fill a route template's `{id}` from the seeded shop.

    A template whose first segment owns no id mapping raises: the alternative
    is a route that silently leaves the sweep.
    """
    first_segment = template.strip("/").split("/", 1)[0]
    if "{" in template and first_segment not in _PARAM_OWNER:
        raise AssertionError(
            f"route {template!r} has a parameter but its first segment "
            f"{first_segment!r} names no seeded entity; add it to _PARAM_OWNER "
            "so the swept route is the real one"
        )
    key = _PARAM_OWNER.get(first_segment)
    if key is None:
        return template
    return re.sub(r"\{[^}]+\}", lambda _: str(context[key]), template)


# ---------------------------------------------------------------------------
# Seeding: a shop with a draft document, a confirmed document, and one return
# ---------------------------------------------------------------------------


def _create_return_through_the_parent(
    page: Page,
    api: ApiClient,
    *,
    parent_path: str,
    parent_id: int,
    action_form: str,
    return_path: str,
) -> int:
    """Create a return draft by clicking, and return its id.

    Neither return family exposes a JSON API (`grep -r 'api/purchase-returns'
    src/` is empty), so the record can only be produced the way an operator
    produces it — through the parent's action, which answers an `HX-Redirect`
    that htmx performs as a real navigation. Waiting on the URL, not on
    `networkidle`, is what makes the landing an assertion: the redirect happens
    after the response the click returned.
    """
    page.goto(f"{api.base_url}{parent_path}/{parent_id}")
    page.wait_for_load_state("networkidle")
    page.locator(f"{action_form} button[type=submit]").click()
    page.wait_for_url(f"**{return_path}/*", timeout=20000)
    page.wait_for_load_state("networkidle")
    return int(urlsplit(page.url).path.rstrip("/").rsplit("/", 1)[1])


def seed_the_shop(page: Page, api: ApiClient) -> dict[str, int]:
    """One account, one product, a draft document pair, a confirmed pair, returns.

    Two confirmed credit parents exist for one reason: a return record has no
    API, and its page can only be swept once a return exists. Credit on both
    sides collects nothing, so the seed writes no payment rows and the return
    it creates is the only document the sweep meets that this seed did not
    create directly.
    """
    data = seed_harness_data(api)

    returnable = create_product(
        api,
        sku="INVARIANT-RETURNABLE",
        name="Invariant Returnable",
        sale_price="25.00",
        cost_price="12.00",
        stock="40",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(returnable["id"])
    supplier_id = create_supplier(api, "Invariant Supplier")
    customer_id = create_customer(api, "Invariant Buyer")

    confirmed_purchase = create_confirmed_credit_purchase(
        api, supplier_id, product_id, qty="5", unit_cost="12.00"
    )
    confirmed_sale = create_confirmed_credit_sale(
        api, customer_id, product_id, qty="4", unit_price="25.00"
    )

    purchase_return_id = _create_return_through_the_parent(
        page,
        api,
        parent_path="/purchases",
        parent_id=confirmed_purchase,
        action_form="#purchase-return-action",
        return_path="/purchase-returns",
    )
    customer_return_id = _create_return_through_the_parent(
        page,
        api,
        parent_path="/sales",
        parent_id=confirmed_sale,
        action_form="#sale-credit-note-action",
        return_path="/customer-returns",
    )

    return {
        "account_id": data.account_id,
        "customer_id": data.customer_id,
        "sale_id": data.sale_id,
        "purchase_id": data.purchase_id,
        "product_id": product_id,
        "purchase_return_id": purchase_return_id,
        "customer_return_id": customer_return_id,
    }


def sweepable_routes(context: dict[str, Any]) -> list[tuple[str, str]]:
    """`(template, concrete path)` for every page route inside the shell."""
    return [
        (template, _instantiate(template, context))
        for template in registered_page_routes()
        if template not in _ANONYMOUS_PAGE_ROUTES
    ]


# ---------------------------------------------------------------------------
# Reading the rendered page
# ---------------------------------------------------------------------------

# The shapes of an internal error, not a list of the types that produce them.
# Each alternative is a *vocabulary*: serde's, Rust's panic machinery's,
# Rust type naming's, and the repository's own filesystem layout. A new error
# type is caught without this list changing.
#
# `SUM(Income)` appears in the dashboard's own copy, which is why the
# PascalCase-call alternative requires a lowercase second letter: an all-caps
# SQL function is not a Rust type name.
_INTERNAL_TEXT_PATTERNS = {
    "serde vocabulary": r"deserializ\w*|serializ\w*",
    "extractor sentence": r"cannot parse|missing field|invalid type",
    "panic machinery": r"\bpanicked\b|\bbacktrace\b|RUST_BACKTRACE",
    "unwrap call": r"\bunwrap\w*\b",
    "Rust error type": r"\b[A-Z][A-Za-z0-9_]*Error\b",
    "Rust generic type": r"\b[A-Za-z_][A-Za-z0-9_:]*<[^<>\n]{1,80}>",
    "Rust enum or constructor": r"\b[A-Z][a-z][A-Za-z0-9_]*\(",
    "repository source path": r"(?:src|tests|e2e)/[\w/.-]+\.(?:rs|py)|\b\w+\.rs:\d+",
    "absolute filesystem path": r"/home/[\w/.-]+|/Users/[\w/.-]+|/tmp/[\w/.-]+",
}

_DUPLICATE_IDS_PROBE = """
() => {
  const counts = {};
  for (const el of document.querySelectorAll('[id]')) {
    counts[el.id] = (counts[el.id] || 0) + 1;
  }
  return Object.entries(counts)
    .filter(([, n]) => n > 1)
    .map(([id, n]) => ({ id, count: n }));
}
"""

# A control is labelled when the label is associated *programmatically*: a
# `<label for>` pointing at it, an `aria-label`, an `aria-labelledby`, or a
# wrapping `<label>`. A `<label>` that merely sits above it looks like a label
# to a person and is invisible to assistive technology, which is exactly the
# gap this reports.
_UNLABELLED_CONTROLS_PROBE = """
() => {
  const unlabelled = [];
  const controls = document.querySelectorAll(
    'input[name], select[name], textarea[name]'
  );
  for (const control of controls) {
    // A hidden input is not a control a person operates, and HTML-AAM does not
    // make it labelable.
    if (control.type === 'hidden') continue;
    const id = control.getAttribute('id');
    const byFor = id
      ? document.querySelector('label[for="' + CSS.escape(id) + '"]')
      : null;
    const aria =
      control.getAttribute('aria-label') ||
      control.getAttribute('aria-labelledby');
    const wrapping = control.closest('label');
    if (byFor || aria || wrapping) continue;
    const visibleLabel = control.previousElementSibling &&
      control.previousElementSibling.tagName === 'LABEL' &&
      (control.previousElementSibling.textContent || '').trim();
    unlabelled.push({
      name: control.name,
      tag: control.tagName.toLowerCase(),
      type: control.type || null,
      id,
      // Distinguishes "no label text anywhere" from "a label a person can read
      // but assistive technology cannot be told about".
      visually_labelled: !!visibleLabel,
    });
  }
  return unlabelled;
}
"""

_SIDEBAR_ENTRIES_PROBE = """
() => Array.from(document.querySelectorAll('#sidebar a[data-nav]')).map((a) => ({
  nav: a.getAttribute('data-nav'),
  href: a.getAttribute('href'),
  target: a.getAttribute('target'),
}))
"""

_ANCHOR_HREFS_PROBE = """
() => Array.from(document.querySelectorAll('a[href]')).map((a) => ({
  href: a.getAttribute('href'),
  target: a.getAttribute('target'),
  mutating: !!(a.getAttribute('hx-post') || a.getAttribute('hx-put') ||
    a.getAttribute('hx-delete') || a.getAttribute('hx-patch')),
}))
"""

_DIALOG_OPENERS_PROBE = """
() => Array.from(document.querySelectorAll('button[onclick*="showModal"]'))
  .filter((el) => el.offsetParent !== null)
  .map((el) => ({
    id: el.id || null,
    onclick: el.getAttribute('onclick'),
  }))
"""


class PageSignals:
    """Console errors and uncaught exceptions, reset per navigation."""

    def __init__(self, page: Page) -> None:
        self._seen: list[str] = []
        page.on("console", self._on_console)
        page.on("pageerror", self._on_pageerror)

    def _on_console(self, message: Any) -> None:
        if message.type == "error":
            self._seen.append(f"console error: {message.text}")

    def _on_pageerror(self, error: Any) -> None:
        self._seen.append(f"uncaught exception: {error}")

    def reset(self) -> None:
        self._seen = []

    @property
    def seen(self) -> list[str]:
        return list(self._seen)


def _visit(page: Page, base_url: str, path: str) -> None:
    page.goto(f"{base_url}{path}")
    page.wait_for_load_state("networkidle")


def _body_text(page: Page) -> str:
    return page.locator("body").inner_text()


def _format_offenders(header: str, offenders: list[str]) -> str:
    return f"{header}\n" + "\n".join(f"  - {line}" for line in offenders)


# ---------------------------------------------------------------------------
# 1. No page logs a console error
# ---------------------------------------------------------------------------


def test_no_page_logs_a_console_error_or_throws(page: Page, api: ApiClient) -> None:
    """A page that half-fails in the browser is a page nobody noticed.

    A missing static asset, a bad fragment target or a thrown handler all show
    up here and nowhere else: the HTTP status is 200, the assertions of every
    other test still find the elements they look for, and the operator sees a
    screen that does not work.
    """
    context = seed_the_shop(page, api)
    signals = PageSignals(page)
    routes = sweepable_routes(context)

    offenders: list[str] = []
    for template, path in routes:
        signals.reset()
        _visit(page, api.base_url, path)
        for message in signals.seen:
            offenders.append(f"{template} ({path}): {message}")

    assert len(routes) >= 15, f"the sweep only covers {len(routes)} routes: {routes}"
    assert not offenders, _format_offenders(
        f"{len(offenders)} browser error(s) across {len(routes)} page loads:",
        offenders,
    )


# ---------------------------------------------------------------------------
# 2. No page renders an internal error
# ---------------------------------------------------------------------------


def test_no_page_renders_an_internal_error_name(page: Page, api: ApiClient) -> None:
    """The rendered text is the last boundary, and it leaks quietly.

    A `Form` rejection is a response the handler never sees, so it escapes
    every `map_err` in the route and lands in the page as serde's own sentence.
    Reading the text a person would read is the only way to catch it, and the
    patterns are the *shapes* of those sentences rather than the names of the
    types that produce them.
    """
    context = seed_the_shop(page, api)
    routes = sweepable_routes(context)

    offenders: list[str] = []
    for template, path in routes:
        _visit(page, api.base_url, path)
        text = _body_text(page)
        for label, pattern in _INTERNAL_TEXT_PATTERNS.items():
            for match in sorted(set(re.findall(pattern, text))):
                offenders.append(f"{template} ({path}) [{label}]: {match!r}")

    assert not offenders, _format_offenders(
        f"{len(offenders)} internal-error fragment(s) reached the screen:",
        offenders,
    )


# ---------------------------------------------------------------------------
# 3. No page carries a duplicate element id
# ---------------------------------------------------------------------------


def test_no_page_has_a_duplicate_element_id(page: Page, api: ApiClient) -> None:
    """A repeated body is invisible to text assertions and visible here.

    When two declarations disagree about which element owns the record body —
    the fragment's `hx-target` and the shell's swap — the response installs a
    second copy of the same fragment inside the first. Every text assertion in
    the suite still finds what it looks for in the fresh copy and stops looking,
    which is how a stale `Paid` chip survived four green suites. Duplicate ids
    are the count that makes the second copy legible.
    """
    context = seed_the_shop(page, api)
    routes = sweepable_routes(context)

    offenders: list[str] = []
    ids_seen = 0
    for template, path in routes:
        _visit(page, api.base_url, path)
        ids_seen += page.locator("[id]").count()
        for duplicate in page.evaluate(_DUPLICATE_IDS_PROBE):
            offenders.append(
                f"{template} ({path}): id {duplicate['id']!r} "
                f"appears {duplicate['count']} times"
            )

    assert ids_seen > 0, "no element carried an id; the probe found nothing to check"
    assert not offenders, _format_offenders(
        f"{len(offenders)} duplicate element id(s) across the app:",
        offenders,
    )


# ---------------------------------------------------------------------------
# 4. Every named form control is labelled
# ---------------------------------------------------------------------------


def test_every_named_form_control_has_a_programmatic_label(
    page: Page, api: ApiClient
) -> None:
    """A label a person can read is not yet a label a screen reader can.

    The house convention is a `<label class="field-label">` sitting above its
    control, and a `<label>` with no `for` is not associated with it: assistive
    technology announces the control as unlabelled. A wrapping `<label>` and an
    `aria-label` both associate; a bare sibling does not. The offense is
    reported with the control's `name`, which is the attribute the server reads,
    so the line names the field rather than a DOM position.
    """
    context = seed_the_shop(page, api)
    routes = sweepable_routes(context)

    offenders: list[str] = []
    controls_seen = 0
    for template, path in routes:
        _visit(page, api.base_url, path)
        unlabelled = page.evaluate(_UNLABELLED_CONTROLS_PROBE)
        controls_seen += page.locator(
            "input[name], select[name], textarea[name]"
        ).count()
        for control in unlabelled:
            kind = (
                "has a visible label that is not associated with it"
                if control["visually_labelled"]
                else "has no label text at all"
            )
            offenders.append(
                f"{template} ({path}): <{control['tag']} "
                f"name={control['name']!r} type={control['type']!r}> {kind}"
            )

    assert controls_seen > 0, "no named control was on any page; nothing was checked"
    assert not offenders, _format_offenders(
        f"{len(offenders)} of {controls_seen} named form control(s) lack a "
        "programmatic label:",
        offenders,
    )


# ---------------------------------------------------------------------------
# 5a. Every sidebar entry navigates where it says it does
# ---------------------------------------------------------------------------


def test_every_sidebar_entry_navigates_to_the_route_it_declares(
    page: Page, api: ApiClient
) -> None:
    """Reachability is a click, not a URL.

    A test that typed `/purchase-returns` would have passed against the build
    that shipped the page with no sidebar entry at all, because the page
    answered 200. This starts where an operator starts and asserts the outcome
    of the click: the entry the shell shows, and the route it opens, are the
    same claim.
    """
    context = seed_the_shop(page, api)
    _visit(page, api.base_url, "/")

    entries = page.evaluate(_SIDEBAR_ENTRIES_PROBE)
    assert entries, "the shell rendered no sidebar entries; nothing was checked"

    offenders: list[str] = []
    for entry in entries:
        if entry["target"] == "_blank":
            # The REST API link opens a JSON response in a new tab; it is not a
            # page of the shell and has no shell invariant to satisfy.
            continue
        page.locator(f'#sidebar a[data-nav="{entry["nav"]}"]').click()
        page.wait_for_load_state("networkidle")
        landed = urlsplit(page.url)
        declared = urlsplit(entry["href"])
        if landed.path != declared.path or landed.fragment != declared.fragment:
            offenders.append(
                f"sidebar entry {entry['nav']!r} declares {entry['href']!r} "
                f"but landed on {landed.path!r} (fragment {landed.fragment!r})"
            )
        _visit(page, api.base_url, "/")

    assert not offenders, _format_offenders(
        f"{len(offenders)} sidebar entry(ies) do not open what they promise:",
        offenders,
    )


# ---------------------------------------------------------------------------
# 5b. Every registered page route is linked from somewhere in the shell
# ---------------------------------------------------------------------------


def test_every_registered_page_route_is_reachable_from_the_shell(
    page: Page, api: ApiClient
) -> None:
    """The other half of the missing-entry defect, and the half that was absent.

    Checking that the entries which *exist* navigate correctly cannot catch a
    route that has no entry at all. Walking the link graph from the dashboard
    can: every registered page route must be reachable by following links a
    person can click. Record pages are followed through the list rows, which
    keep their `href` as the no-JavaScript way to read the same document.
    """
    context = seed_the_shop(page, api)
    expected = {path for _, path in sweepable_routes(context)}

    reachable: set[str] = set()
    queue = ["/"]
    while queue:
        path = queue.pop(0)
        if path in reachable or path not in expected:
            continue
        reachable.add(path)
        _visit(page, api.base_url, path)
        for anchor in page.evaluate(_ANCHOR_HREFS_PROBE):
            if anchor["target"] == "_blank" or anchor["mutating"]:
                continue
            href = anchor["href"]
            if not href or href.startswith("#"):
                continue
            candidate = urlsplit(href)
            if candidate.netloc and candidate.netloc != urlsplit(api.base_url).netloc:
                continue
            if candidate.path in expected and candidate.path not in reachable:
                queue.append(candidate.path)

    missing = sorted(expected - reachable)
    assert not missing, (
        "these registered page routes answer 200 but no click reaches them, so "
        "the only way in is typing the URL:\n"
        + "\n".join(f"  - {path}" for path in missing)
    )


# ---------------------------------------------------------------------------
# 5c. Every record page's action responds
# ---------------------------------------------------------------------------


def test_every_record_page_action_opens_what_it_promises(
    page: Page, api: ApiClient
) -> None:
    """A record page's actions are buttons that promise something specific.

    Defect two lived here: the creation dialog promised a picker and delivered
    a text field whose only possible answer was a 422. Clicking the opener and
    asserting the dialog actually opens catches the promise that never lands,
    without knowing which dialog any page is supposed to have.

    **Only dialog openers are clicked**, and that boundary is deliberate: a
    submit that writes (`hx-post`, a form submit) would make the net mutate the
    shop it measures, and a measurement that writes is a measurement nobody can
    trust. The writing paths are covered by the departmental suites, which seed
    for exactly the write they intend.
    """
    context = seed_the_shop(page, api)
    signals = PageSignals(page)

    record_routes = [
        (template, path)
        for template, path in sweepable_routes(context)
        if "{" in template
    ]
    assert record_routes, "no parameterised route was registered; nothing was checked"

    offenders: list[str] = []
    openers_clicked = 0
    for template, path in record_routes:
        signals.reset()
        _visit(page, api.base_url, path)
        count = page.locator('button[onclick*="showModal"]').count()
        for index in range(count):
            opener = page.evaluate(_DIALOG_OPENERS_PROBE)[index]
            # A record page can render with its drawer already open — the deep
            # link `/customers/{id}` IS the no-JavaScript route to the customer
            # statement — and an open drawer overlays the list that owns the
            # opener, so the click is refused by the overlay rather than by the
            # button. Escape is base.html's own dismissal key for the drawer;
            # pressing it first makes the click land on the opener.
            page.keyboard.press("Escape")
            page.locator('button[onclick*="showModal"]').nth(index).click()
            openers_clicked += 1
            dialog = page.locator("dialog[open]")
            if dialog.count() != 1:
                offenders.append(
                    f"{template} ({path}): the action "
                    f"{opener['id'] or opener['onclick']!r} left {dialog.count()} "
                    "open dialogs (expected 1)"
                )
            else:
                page.keyboard.press("Escape")
                if dialog.count() != 0:
                    offenders.append(
                        f"{template} ({path}): the dialog opened by "
                        f"{opener['id'] or opener['onclick']!r} did not close on Escape"
                    )
        for message in signals.seen:
            offenders.append(f"{template} ({path}): {message}")

    assert openers_clicked > 0, (
        "no record page rendered a dialog opener; the probe found nothing to click"
    )
    assert not offenders, _format_offenders(
        f"{len(offenders)} record action(s) did not respond as promised:",
        offenders,
    )


# ---------------------------------------------------------------------------
# The exclusion, asserted
# ---------------------------------------------------------------------------


def _anonymous_routes_are_registered() -> Iterator[str]:
    for route in registered_page_routes():
        if route in _ANONYMOUS_PAGE_ROUTES:
            yield route


def test_the_routes_outside_the_shell_are_registered_and_redirect(
    page: Page, api: ApiClient
) -> None:
    """The two exclusions are a claim, so they are measured.

    `/login` and `/setup` are excluded from the sweep because a signed-in
    context cannot see them. This pins both facts: they are still registered,
    and asking for them from inside the shell lands somewhere else. If either
    route disappears, or stops redirecting, the exclusion stops being true and
    this test says so.
    """
    registered = set(_anonymous_routes_are_registered())
    assert registered == _ANONYMOUS_PAGE_ROUTES, (
        "the routes outside the shell changed: registered "
        f"{sorted(registered)}, excluded {sorted(_ANONYMOUS_PAGE_ROUTES)}"
    )

    for path in sorted(_ANONYMOUS_PAGE_ROUTES):
        _visit(page, api.base_url, path)
        landed = urlsplit(page.url).path
        assert landed != path, (
            f"{path} rendered inside the signed-in session (landed on {landed}); "
            "the exclusion from the sweep is no longer true"
        )
