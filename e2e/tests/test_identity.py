"""Slice S1b: the login gate, proven from the browser's side.

The Rust suite proves the guard and the session routes in-process; these tests
prove what only a real browser or the real environment can:

- an unauthenticated visit is redirected to the login page, and logging in
  through the real form reaches the dashboard;
- a wrong password keeps the visitor on the form with the generic error;
- ``ROYA_COOKIE_SECURE=1`` reaches the real ``Set-Cookie`` wire. ``main`` has
  no Rust test, so the harness is the only place that environment wiring can
  be exercised end to end.

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

    page.get_by_label("Usuario").fill(TEST_ADMIN_USERNAME)
    page.get_by_label("Contraseña").fill(TEST_ADMIN_PASSWORD)
    page.get_by_role("button", name="Iniciar sesión").click()

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
    page.get_by_label("Usuario").fill(TEST_ADMIN_USERNAME)
    page.get_by_label("Contraseña").fill("definitely-not-the-password")
    page.get_by_role("button", name="Iniciar sesión").click()

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
            "ROYA_ADMIN_PASSWORD": TEST_ADMIN_PASSWORD,
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
            cookie = _session_set_cookie(
                server.url, TEST_ADMIN_USERNAME, TEST_ADMIN_PASSWORD
            )
            assert cookie.startswith("roya_session="), cookie
            assert "; Secure" in cookie, cookie
            assert "HttpOnly" in cookie, cookie
        finally:
            server.stop()
