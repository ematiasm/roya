"""Slice S1b + S8 part 1: the identity gate, proven from the browser's side.

The Rust suite proves the guard and the session routes in-process; these tests
prove what only a real browser or the real environment can:

- an unauthenticated visit is redirected to the login page, and logging in
  through the real form reaches the dashboard (S1b);
- a wrong password keeps the visitor on the form with the generic error;
- ``ROYA_COOKIE_SECURE=1`` reaches the real ``Set-Cookie`` wire. ``main`` has
  no Rust test, so the harness is the only place that environment wiring can
  be exercised end to end (S1b);
- a user created through the real users screen is flagged
  ``must_change_password`` and its first login is confined to ``/password``:
  typing another screen's URL bounces back to the change form, and completing
  the change lands on the dashboard (S8 part 1, AC22);
- an HTMX request issued after the session row was expired server-side is
  answered ``401`` with ``HX-Redirect: /login``, and htmx 1.9.12 performs the
  navigation itself — the claim no Rust test can make, because only the
  browser decides what it does with a correct server response (S8 part 1);
- a principal whose role holds ``sales.read`` but not ``sales.create``
  submits the New Sale HTMX form and the refusal reaches the operator as the
  notice box the global ``htmx:responseError`` handler renders, while the DOM
  never swaps as if the action had succeeded (S8 part 1, AC22).

Every other test in the suite rides the session the harness logged in with
once per server (see ``conftest``), so they never see the gate; the tests here
use an anonymous context on purpose.
"""

from __future__ import annotations

import json
import os
import subprocess
import urllib.request
from collections.abc import Iterator
from urllib.parse import urlparse

import pytest
from playwright.sync_api import Browser, BrowserContext, Page, expect

from conftest import (
    REPO_ROOT,
    TEST_ADMIN_PASSWORD,
    TEST_ADMIN_USERNAME,
    LiveServer,
    _free_port,
    _wait_until_ready,
)
from helpers import ApiClient, e2e_copy, expire_session_in_database, setup_fresh_server


@pytest.fixture
def anonymous_page(
    browser: Browser,
    browser_context_args: dict,
    live_server: LiveServer,
) -> Iterator[Page]:
    """A page whose context carries no session cookie.

    The shared ``page`` fixture injects the harness's session so the rest of
    the suite never trips the gate; gate-behaviour tests need the opposite, an
    unauthenticated visitor, and build it here instead of logging out.
    """
    context = browser.new_context(**browser_context_args)
    page = context.new_page()
    try:
        yield page
    finally:
        context.close()


def test_an_unauthenticated_browser_is_redirected_and_the_form_login_reaches_the_dashboard(
    anonymous_page: Page, live_server: LiveServer
) -> None:
    """The gate is real: no session means the login page, and the form gets in.

    The Rust suite proves the guard's decision; the browser proves the whole
    round trip a visitor experiences — the redirect, the real form submit, the
    cookie, and the dashboard on the other side.
    """
    page = anonymous_page
    page.goto(f"{live_server.url}/")
    assert "/login" in page.url, f"expected the login page, got {page.url}"

    page.get_by_label(e2e_copy("username")).fill(TEST_ADMIN_USERNAME)
    page.get_by_label(e2e_copy("password")).fill(TEST_ADMIN_PASSWORD)
    page.get_by_role("button", name=e2e_copy("sign_in")).click()

    expect(page).to_have_url(f"{live_server.url}/")
    expect(page.locator("#total-balance")).to_be_visible()


def test_a_wrong_password_stays_on_the_login_form_with_the_generic_error(
    anonymous_page: Page, live_server: LiveServer
) -> None:
    """A failed login re-renders the form with the one generic message.

    The visitor must not be dropped on a blank page or a raw error: the form
    is still there to try again, and nothing reveals which part was wrong.
    """
    page = anonymous_page
    page.goto(f"{live_server.url}/login")
    page.get_by_label(e2e_copy("username")).fill(TEST_ADMIN_USERNAME)
    page.get_by_label(e2e_copy("password")).fill("definitely-not-the-password")
    page.get_by_role("button", name=e2e_copy("sign_in")).click()

    expect(page.locator("[data-notice='error']")).to_be_visible()
    expect(page.locator("#username")).to_be_visible()


def _session_set_cookie(base_url: str, username: str, password: str) -> str:
    """POST /api/sessions and return the raw ``Set-Cookie`` header."""
    request = urllib.request.Request(
        f"{base_url}/api/sessions",
        data=json.dumps({"username": username, "password": password}).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return response.headers.get("Set-Cookie", "")


def test_the_default_wire_cookie_carries_no_secure_flag(live_server: LiveServer) -> None:
    """Without the env flag the login cookie is session-scoped over plain HTTP.

    ``Secure`` would stop a browser from storing the cookie on ``http://``,
    so the default harness server must not emit it. ``HttpOnly`` is asserted
    too: it is the flag that stays on no matter the environment.
    """
    cookie = _session_set_cookie(
        live_server.url, TEST_ADMIN_USERNAME, TEST_ADMIN_PASSWORD
    )
    assert cookie.startswith("roya_session="), cookie
    assert "; Secure" not in cookie, cookie
    assert "HttpOnly" in cookie, cookie


def test_roya_cookie_secure_1_puts_secure_on_the_real_wire(
    roya_binary, tmp_path
) -> None:
    """A server spawned with ROYA_COOKIE_SECURE=1 must set ``Secure``.

    The variable is read in ``main`` and there is no Rust test of ``main``,
    so a second real server with the flag is the honest way to prove the
    environment reaches the cookie policy on the wire.
    """
    port = _free_port()
    db_path = tmp_path / "roya.db"
    log_path = tmp_path / "secure-server.log"
    environment = os.environ.copy()
    environment.update(
        {
            "DATABASE_URL": f"sqlite://{db_path}",
            "PORT": str(port),
            "RUST_LOG": "info",
            "ROYA_COOKIE_SECURE": "1",
        }
    )
    with log_path.open("w", encoding="utf-8") as log_file:
        process = subprocess.Popen(
            [str(roya_binary)],
            cwd=str(REPO_ROOT),
            env=environment,
            stdout=log_file,
            stderr=subprocess.STDOUT,
        )
        server = LiveServer(
            url=f"http://127.0.0.1:{port}",
            port=port,
            db_path=db_path,
            log_path=log_path,
            process=process,
        )
        try:
            _wait_until_ready(server)
            setup_fresh_server(ApiClient(server.url))
            cookie = _session_set_cookie(
                server.url, TEST_ADMIN_USERNAME, TEST_ADMIN_PASSWORD
            )
            assert cookie.startswith("roya_session="), cookie
            assert "; Secure" in cookie, cookie
            assert "HttpOnly" in cookie, cookie
        finally:
            server.stop()


# ---------------------------------------------------------------------------
# S8 part 1: the rest of AC22, through the real screens and a real browser
# ---------------------------------------------------------------------------

# The initial credential the users screen assigns, and the one the confined
# session changes to. Both are ≥ 12 characters (the service's minimum).
INITIAL_PASSWORD = "first-password-123"
CHANGED_PASSWORD = "changed-password-456"


def _response_for(path: str, method: str = "GET"):
    """Predicate matching one request path and method, query string ignored."""

    def matches(response) -> bool:
        return urlparse(response.url).path == path and response.request.method == method

    return matches


def _log_in_through_the_form(
    page: Page, live_server: LiveServer, username: str, password: str
) -> None:
    """Drive the real login form; the caller asserts where the browser lands."""
    page.goto(f"{live_server.url}/login")
    page.get_by_label(e2e_copy("username")).fill(username)
    page.get_by_label(e2e_copy("password")).fill(password)
    page.get_by_role("button", name=e2e_copy("sign_in")).click()


def _create_user_through_the_screen(
    page: Page, *, username: str, display_name: str, password: str
) -> None:
    """Create a user through the users screen's real dialog, and read it back.

    Creating through the screen is the point: the screen is what sets
    ``must_change_password``, and the refreshed list is the effect read back.
    """
    page.get_by_role("button", name=e2e_copy("new_user")).click()
    dialog = page.locator("#new-user-dialog")
    dialog.locator('input[name="username"]').fill(username)
    dialog.locator('input[name="display_name"]').fill(display_name)
    dialog.locator('input[name="password"]').fill(password)
    with page.expect_response(_response_for("/web/users", "POST")):
        dialog.get_by_role("button", name=e2e_copy("create_user")).click()
    expect(page.locator("#user-list")).to_contain_text(username)
    expect(page.locator("#user-list")).to_contain_text(e2e_copy("must_change"))


def _assign_role_through_the_screen(
    page: Page, *, username: str, role_name: str
) -> None:
    """Grant one role to a user through the users screen's Roles dialog.

    The button is scoped to the CREATED user's row: the acting administrator's
    own row hides the control, but the migration's ``sistema`` sentinel is a
    real (inactive, roleless) account row with one too, so the click must name
    whose row it means instead of assuming a single match.
    """
    page.locator("#user-list-inner > div > div", has_text=username).locator(
        f'button[aria-label="{e2e_copy("assign_roles")}"]'
    ).click()
    dialog = page.locator("#user-edit-dialog")
    expect(dialog).to_contain_text(f"Roles — {username}")
    dialog.locator("label", has_text=role_name).locator(
        'input[name="role_ids"]'
    ).check()
    with page.expect_response(_response_for("/web/users/roles", "POST")):
        dialog.get_by_role("button", name=e2e_copy("save_roles")).click()
    expect(page.locator("#user-list")).to_contain_text(role_name)


def _change_confined_password(page: Page, *, current: str, new: str) -> None:
    """Complete the confined password change through the real form."""
    page.get_by_label(e2e_copy("current_password")).fill(current)
    # exact=True: the confirm field's label contains this one as a substring.
    page.get_by_label(e2e_copy("new_password"), exact=True).fill(new)
    page.get_by_label(e2e_copy("confirm_password")).fill(new)
    page.get_by_role("button", name=e2e_copy("save_password")).click()


def test_a_created_user_is_confined_to_the_password_change_until_it_changes_it(
    page: Page, anonymous_page: Page, live_server: LiveServer
) -> None:
    """AC22: the forced password change, proven from the browser.

    The Rust suite proves the confinement decision per request; the browser
    proves the flow a flagged operator lives: the first login lands confined,
    another screen's URL typed by hand bounces back to the change form (the
    part no header assertion can prove — only a navigation is a navigation),
    and completing the change lifts the confinement on the same session.
    """
    url = live_server.url

    # The seed is the real screens: create the user (the screen sets
    # must_change_password) and grant it a role, so the unconfined operator
    # may actually reach the app afterwards.
    page.goto(f"{url}/users")
    _create_user_through_the_screen(
        page, username="caja1", display_name="Caja Uno", password=INITIAL_PASSWORD
    )
    _assign_role_through_the_screen(page, username="caja1", role_name="Salesperson")

    # First login: the flag confines the session to the change form.
    visitor = anonymous_page
    _log_in_through_the_form(visitor, live_server, "caja1", INITIAL_PASSWORD)
    expect(visitor).to_have_url(f"{url}/password")
    expect(visitor.get_by_text(e2e_copy("confined"))).to_be_visible()

    # The confinement is real against a URL typed by hand, not only against
    # the app's own links: /products is bounced back to /password.
    visitor.goto(f"{url}/products")
    expect(visitor).to_have_url(f"{url}/password")

    # The change lands on the dashboard, on the same session.
    _change_confined_password(
        visitor, current=INITIAL_PASSWORD, new=CHANGED_PASSWORD
    )
    expect(visitor).to_have_url(f"{url}/")
    expect(visitor.locator("#total-balance")).to_be_visible()

    # And the same session now reaches the screen it could not reach before:
    # the flag is lifted, not merely the redirect avoided.
    visitor.goto(f"{url}/products")
    expect(visitor.locator("#product-list")).to_be_visible()


def test_an_htmx_request_into_an_expired_session_navigates_to_the_login_page(
    anonymous_page: Page, live_server: LiveServer
) -> None:
    """AC22 + the S1b debt: session expiry mid-HTMX, proven by the navigation.

    The server answers the expired session's HTMX request with ``401`` and
    ``HX-Redirect: /login``; the claim this test makes is what htmx 1.9.12
    does with that response in a real browser — it navigates. The session is
    expired server-side (the row's ``expires_at`` is moved into the past in
    the throwaway database), so the refusal is the server's own decision and
    the cookie in the browser is untouched.
    """
    url = live_server.url
    visitor = anonymous_page

    # Log in through the real form: this test owns the session it expires.
    _log_in_through_the_form(visitor, live_server, TEST_ADMIN_USERNAME, TEST_ADMIN_PASSWORD)
    expect(visitor.locator("#total-balance")).to_be_visible()

    # A screen whose refresh control issues a real HTMX request.
    visitor.goto(f"{url}/users")
    expect(visitor.locator("#user-list")).to_contain_text(TEST_ADMIN_USERNAME)

    # Expire the row the way the AC means it: server-side, cookie untouched.
    cookies = {c["name"]: c["value"] for c in visitor.context.cookies()}
    expire_session_in_database(live_server.db_path, cookies["roya_session"])

    with visitor.expect_response(_response_for("/web/users", "GET")) as response_info:
        visitor.get_by_role("button", name=e2e_copy("refresh")).click()
    response = response_info.value
    assert response.status == 401, "the expired session must be refused in its HTMX shape"
    assert response.headers.get("hx-redirect") == "/login", response.headers

    # The claim no Rust test can make: htmx 1.9.12 performed the navigation.
    expect(visitor).to_have_url(f"{url}/login")
    expect(visitor.get_by_label("Username")).to_be_visible()


def test_a_permission_denied_htmx_form_reaches_the_notice_and_never_swaps(
    page: Page, anonymous_page: Page, live_server: LiveServer
) -> None:
    """AC22: a permission-denied HTMX form, through the browser.

    A principal whose only permission is ``sales.read`` submits the New Sale
    HTMX form. The refusal reaches the operator as the app's notice — the
    global ``htmx:responseError`` handler renders the JSON error into the
    ``#notice`` box — and the DOM never swaps as if the action had succeeded
    (the success path answers ``HX-Redirect`` to the new record, so the URL
    would have moved).
    """
    url = live_server.url

    # -- the limited principal, built through the real screens ----------------
    # A role whose matrix holds exactly sales.read, ticked through the roles
    # screen's matrix editor; saving replaces the whole set.
    page.goto(f"{url}/roles")
    page.get_by_role("button", name=e2e_copy("new_role")).click()
    new_role = page.locator("#new-role-dialog")
    new_role.locator('input[name="code"]').fill("solo_consulta")
    new_role.locator('input[name="name"]').fill("Sólo consulta")
    new_role.locator('input[name="description"]').fill("Ver ventas; ninguna otra acción.")
    with page.expect_response(_response_for("/web/roles", "POST")):
        new_role.get_by_role("button", name=e2e_copy("create_role")).click()
    expect(page.locator("#role-list")).to_contain_text("solo_consulta")

    role_row = page.locator("#role-list-inner > div > div", has_text="solo_consulta")
    role_row.locator(f'button[aria-label="{e2e_copy("edit_role")}"]').click()
    edit_dialog = page.locator("#role-edit-dialog")
    expect(edit_dialog).to_contain_text("solo_consulta")
    edit_dialog.locator("label", has_text="sales.read").locator(
        'input[name="permission_ids"]'
    ).check()
    with page.expect_response(_response_for("/web/roles/matrix", "POST")):
        edit_dialog.get_by_role("button", name=e2e_copy("save_permissions")).click()

    page.goto(f"{url}/users")
    _create_user_through_the_screen(
        page,
        username="consulta1",
        display_name="Consulta Uno",
        password=INITIAL_PASSWORD,
    )
    _assign_role_through_the_screen(
        page, username="consulta1", role_name="Sólo consulta"
    )

    # -- the limited operator logs in and changes its own password first ------
    # A screen-created user is flagged, so the first login is confined. After
    # the change the login redirect lands on the dashboard route, which this
    # principal cannot read: the full-page refusal card is what it sees.
    visitor = anonymous_page
    _log_in_through_the_form(visitor, live_server, "consulta1", INITIAL_PASSWORD)
    expect(visitor).to_have_url(f"{url}/password")
    _change_confined_password(
        visitor, current=INITIAL_PASSWORD, new=CHANGED_PASSWORD
    )
    expect(visitor.locator("[data-notice='error']")).to_contain_text(
        e2e_copy("permission_required")
    )

    # -- the denied HTMX form --------------------------------------------------
    # The sales screen is readable (sales.read); the New Sale form is offered
    # by the markup, and the handler is what refuses.
    visitor.goto(f"{url}/sales")
    expect(visitor.locator("#sale-list")).to_be_visible()

    form = visitor.locator('#new-sale form[data-action="Create sale"]')
    form.locator("select[name='customer_id']").select_option(label="Consumidor final")
    with visitor.expect_response(_response_for("/web/sales", "POST")) as response_info:
        form.get_by_role("button", name="Create Draft").click()
    response = response_info.value
    assert response.status == 403, "the action must be refused at the handler"
    assert "sales.create" in response.json()["error"], response.json()

    notice = visitor.locator("[data-notice='error']")
    expect(notice).to_contain_text("Create sale failed")
    expect(notice).to_contain_text(
        "Se necesita el permiso «sales.create» para esta acción"
    )
    # No swap as if it had succeeded: the success answer is an HX-Redirect to
    # the new record, so a browser that had swapped would have left this URL.
    expect(visitor).to_have_url(f"{url}/sales")
