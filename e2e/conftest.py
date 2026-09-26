"""Fixtures for the roya browser suite (slice E1: the harness).

The suite drives the real binary against a throwaway SQLite file on a free
port. It never touches the development database, and it proves it: after the
server answers, its own log must name the throwaway file and the development
database must be unchanged. It never sleeps past a debounce, and a failing test
leaves a Playwright trace, a screenshot and the captured server log under
``e2e/.artifacts/<test name>/``.

There are two server states, and they are two fixtures rather than one fixture
plus a mid-test fix-up, because the difference between them is the whole first-
run lifecycle. ``live_server`` is a spawned server whose setup has been completed
and which the harness has logged in - the state the rest of the suite rides.
``first_run_server`` is an independently spawned server whose setup has *not* been
completed: no ``business_settings`` row, no session, no login. The one-time
``/setup`` wizard exists only in that second state, and it is reached by asking
for the server that genuinely holds it.

The fixtures live at the suite root so ``tests/`` can stay a flat directory of
plain modules.
"""

from __future__ import annotations

import contextlib
import os
import re
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator

import pytest
from playwright.sync_api import Browser, BrowserContext, Page

from helpers import (
    E2E_ADMIN_PASSWORD,
    E2E_ADMIN_USERNAME,
    ApiClient,
    SeedError,
    setup_fresh_server,
)

REPO_ROOT = Path(__file__).resolve().parent.parent
E2E_ROOT = Path(__file__).resolve().parent
ARTIFACTS_ROOT = E2E_ROOT / ".artifacts"

# How long the process may take to answer its first request before the harness
# declares it dead. Generous, because it includes applying migrations.
READY_TIMEOUT_SECONDS = 30.0
# How long teardown waits for the operating system to stop accepting connections
# on the port the server held. This is the observable proof the port is released.
PORT_RELEASE_TIMEOUT_SECONDS = 5.0
# Readiness and release are polled, never slept through for a fixed duration.
_POLL_INTERVAL_SECONDS = 0.1


# ---------------------------------------------------------------------------
# Small process/socket helpers
# ---------------------------------------------------------------------------


def _free_port() -> int:
    """Ask the operating system for an unused port, then give it back.

    The gap between closing this socket and the server binding it is a small
    race, acceptable on a developer machine and noted here rather than hidden.
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def _port_accepts_connections(port: int, *, timeout: float = 0.2) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.settimeout(timeout)
        return probe.connect_ex(("127.0.0.1", port)) == 0


@dataclass
class LiveServer:
    """A spawned roya process, its URL, its database and its captured log."""

    url: str
    port: int
    db_path: Path
    log_path: Path
    process: subprocess.Popen
    # The (name, value) session cookie the harness earned by logging in once
    # after readiness; both clients (the API seeder and the browser context)
    # share it, so no test ever logs in itself.
    session_cookie: tuple[str, str] | None = None

    _stopped: bool = False

    @property
    def log_text(self) -> str:
        try:
            return self.log_path.read_text(encoding="utf-8", errors="replace")
        except FileNotFoundError:
            return ""

    def stop(self) -> None:
        """Terminate the server, wait for it, and prove the port came back."""
        if self._stopped:
            return
        self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=10)
        self._stopped = True
        if not self.wait_until_released():
            raise RuntimeError(
                f"port {self.port} still accepts connections "
                f"{PORT_RELEASE_TIMEOUT_SECONDS}s after the server was stopped; "
                f"the port was not released.\n--- server log ---\n{self.log_text}"
            )

    def wait_until_released(self) -> bool:
        """Poll until nothing accepts connections on the port, or time runs out."""
        deadline = time.monotonic() + PORT_RELEASE_TIMEOUT_SECONDS
        while time.monotonic() < deadline:
            if not _port_accepts_connections(self.port):
                return True
            time.sleep(_POLL_INTERVAL_SECONDS)
        return False


def _wait_until_ready(server: LiveServer) -> None:
    """Poll the root until it answers, instead of sleeping for a guessed time."""
    deadline = time.monotonic() + READY_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if server.process.poll() is not None:
            raise RuntimeError(
                f"roya exited with code {server.process.returncode} before it "
                f"answered.\n--- server log ---\n{server.log_text}"
            )
        try:
            with urllib.request.urlopen(f"{server.url}/", timeout=1.0) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
            pass
        time.sleep(_POLL_INTERVAL_SECONDS)
    raise RuntimeError(
        f"roya did not answer {server.url}/ within {READY_TIMEOUT_SECONDS}s."
        f"\n--- server log ---\n{server.log_text}"
    )


# ---------------------------------------------------------------------------
# Server fixtures
# ---------------------------------------------------------------------------

# Fixed credentials created by the first-run setup form. They are shared by the
# setup helper and the harness login so no test scrapes generated credentials.
TEST_ADMIN_USERNAME = E2E_ADMIN_USERNAME
TEST_ADMIN_PASSWORD = E2E_ADMIN_PASSWORD


def _login_session(server: LiveServer) -> tuple[str, str]:
    """Log the harness in through the real JSON endpoint, once per server."""
    client = ApiClient(server.url)
    return client.login(TEST_ADMIN_USERNAME, TEST_ADMIN_PASSWORD)

# The development database the harness must never open. Its state is captured
# before each spawn so the spawned server can be proven not to have written it.
DEV_DATABASE_PATH = REPO_ROOT / "roya.db"


def _database_stat(path: Path) -> tuple[int, int] | None:
    """Size and modification time of a database file, or None when it is absent."""
    try:
        stat = path.stat()
    except FileNotFoundError:
        return None
    return (stat.st_size, stat.st_mtime_ns)


def _assert_throwaway_database(
    server: LiveServer, dev_before: tuple[int, int] | None
) -> None:
    """Prove the spawned server opened the throwaway file, not another database.

    The strongest signal is the server's own log line: it prints the
    ``database_url`` it hands to the connection pool, so seeing the temporary
    path there is the running process naming the database it opened, from the
    very variable that initialises the pool. Two filesystem checks corroborate
    it: the throwaway file exists and is non-empty (the server created it and
    applied the migrations), and the development database keeps the same size and
    mtime, so a server that silently opened ``roya.db`` cannot pass.
    """
    temp_path = str(server.db_path)
    log = server.log_text
    if temp_path not in log:
        raise RuntimeError(
            "the spawned server did not name the throwaway database in its own log; it "
            "may have opened another one. "
            f"expected {temp_path!r} in:\n{log}"
        )
    try:
        size = server.db_path.stat().st_size
    except FileNotFoundError:
        raise RuntimeError(
            f"the server answered but never created the throwaway database {temp_path}"
        ) from None
    if size == 0:
        raise RuntimeError(
            f"the throwaway database {temp_path} exists but is empty; the server did "
            "not apply the migrations to it"
        )
    dev_after = _database_stat(DEV_DATABASE_PATH)
    if dev_before is not None and dev_after != dev_before:
        raise RuntimeError(
            "the development database `roya.db` changed while the server ran; the "
            f"harness is not isolated.\nbefore={dev_before!r} after={dev_after!r}"
        )


@pytest.fixture(scope="session")
def roya_binary() -> Path:
    """Build the application once per session, then spawn the binary directly.

    Spawning the binary keeps the test run out of `cargo run`'s terminal and
    build lock. The build is the only part that needs the Rust toolchain.
    """
    subprocess.run(["cargo", "build", "--bin", "roya"], cwd=REPO_ROOT, check=True)
    suffix = ".exe" if sys.platform == "win32" else ""
    binary = REPO_ROOT / "target" / "debug" / f"roya{suffix}"
    if not binary.is_file():
        raise RuntimeError(f"cargo build did not produce {binary}")
    return binary


@contextlib.contextmanager
def _spawned_server(
    roya_binary: Path,
    tmp_path: Path,
    label: str,
) -> Iterator[LiveServer]:
    """Spawn one real server on a free port against its own throwaway database.

    The whole spawn lifecycle, extracted so both server states share one copy: the
    port, the environment, the throwaway file, the readiness poll, the guard and
    the teardown that proves the port came back.

    **`label` is the only thing keeping two servers off one SQLite file.** Every
    spawn resolves inside the same function-scoped `tmp_path`, so a repeated
    label would hand both processes the same database path, and the second
    server would be looking at the first one's install - a completed install's
    `business_settings` row, its sessions, its rows. Nothing else in this
    function would notice: the free port and the readiness poll are per-process,
    and the throwaway guard proves *"a throwaway file"*, not *"its own throwaway
    file"* - it reads the path this call was given, so a shared path is trivially
    satisfied. A new server state must therefore pass a label of its own, and it
    must say in its docstring which file is its own.

    The failure mode is loud rather than silent, which is the only reason this is
    a comment and not a guard: with one shared file, the first-run server finds
    the completed install's row, `GET /setup` answers `303` to `/login`, and the
    URL assertion in every first-run test fires with the redirect in its message.
    """

    port = _free_port()
    db_path = tmp_path / f"roya-{label}.db"
    log_path = tmp_path / f"{label}-server.log"

    # Explicit env wins over the repo `.env` (dotenvy does not override what is
    # already set), so a test run can never open the development database.
    environment = os.environ.copy()
    environment.update(
        {
            "DATABASE_URL": f"sqlite://{db_path}",
            "PORT": str(port),
            "RUST_LOG": "info",
        }
    )

    # Capture the development database before the spawn; the guard compares it
    # after readiness to prove the server wrote only the throwaway file.
    dev_before = _database_stat(DEV_DATABASE_PATH)

    log_file = log_path.open("w", encoding="utf-8")
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
        _assert_throwaway_database(server, dev_before)
    except Exception:
        process.kill()
        process.wait(timeout=10)
        log_file.close()
        raise

    try:
        yield server
    finally:
        try:
            server.stop()
        finally:
            log_file.close()


@pytest.fixture
def _pending_install_server(roya_binary: Path, tmp_path: Path) -> Iterator[LiveServer]:
    """A spawned server whose first-run setup has not been completed.

    This is the spawn and nothing else, and that is the point: a server that has
    applied its migrations and answered is a genuine brand-new installation, with
    no `business_settings` row, no session and no login. `/setup` is one-time, so
    this is the only state in which the wizard renders at all - a server whose
    setup is complete answers `GET /setup` with a redirect to `/login`.

    It is the building block `live_server` is made of. A test that needs a fresh
    installation *beside* a completed one must not ask for both, because pytest
    caches a fixture per test: `live_server` is this very object, so the two would
    be one server with its setup already done. That test asks for
    `first_run_server`, which is an independent spawn.
    """
    with _spawned_server(roya_binary, tmp_path, "pending-install") as server:
        yield server


@pytest.fixture
def live_server(_pending_install_server: LiveServer) -> LiveServer:
    """A pending-install server plus the real setup form plus a harness login.

    Startup no longer creates an administrator, so the real first-run form is
    completed before the login, before any test body runs or seed helper can use
    the shared session. Those two steps, in that order, on top of the spawn, are
    the whole behaviour this fixture had when the three lived in one body; the
    spawn itself is `_pending_install_server`, unchanged.
    """
    setup_fresh_server(ApiClient(_pending_install_server.url))
    _pending_install_server.session_cookie = _login_session(_pending_install_server)
    return _pending_install_server


@pytest.fixture
def first_run_server(roya_binary: Path, tmp_path: Path) -> Iterator[LiveServer]:
    """A second, independent server whose first-run setup is not completed.

    The same state the pending-install server has before `live_server` completes
    it - no `business_settings` row, no session, no login, the real wizard
    reachable - spawned independently of it, on its own free port and its own
    throwaway database file, with the same guard around readiness.

    It is a separate fixture rather than the one `live_server` is built from
    because pytest caches a fixture per test, and the test that needs both states
    at once is the visual baseline: it captures `/setup` from a fresh
    installation while every other page comes from the shared session. Composed
    on one fixture, the two would be a single server with its setup already
    complete - measured, not assumed: the browser landed on `/login` instead of
    the wizard, because the "fresh" server had the harness's session cookie and a
    configured business behind it.
    """
    with _spawned_server(roya_binary, tmp_path, "first-run") as server:
        yield server


@pytest.fixture
def api(live_server: LiveServer) -> ApiClient:
    """HTTP client for seeding, on the same endpoints the browser uses.

    It shares the session the harness logged in with, so seeding runs behind
    the same gate the browser does.
    """
    return ApiClient(live_server.url, session_cookie=live_server.session_cookie)


# ---------------------------------------------------------------------------
# Browser fixture with failure artifacts
# ---------------------------------------------------------------------------


def _test_failed(request: pytest.FixtureRequest) -> bool:
    report = getattr(request.node, "rep_call", None)
    # No call report means the test never reached its body (for example a
    # teardown that already raised); keeping the evidence is the safe default.
    return True if report is None else bool(report.failed)


def _artifact_directory(request: pytest.FixtureRequest) -> Path:
    safe_name = re.sub(r"[^A-Za-z0-9._\[\]-]+", "-", request.node.name)
    return ARTIFACTS_ROOT / safe_name


def _write_failure_artifacts(
    request: pytest.FixtureRequest,
    page: Page,
    context: BrowserContext,
    server: LiveServer,
    *,
    prefix: str = "",
) -> None:
    """Write the trace, the screenshot and `server`'s own log for a failed test.

    `prefix` names the files after the server that produced them, and it exists
    because a test may exercise more than one: a test holding both a completed
    install and a fresh one has two contexts, both traced, both failing into the
    *same* per-test directory. Without a prefix the second teardown overwrites the
    first, and what survives is whichever server happened to be finalized last -
    the wrong log and the wrong screenshot for whatever actually failed. A test
    that needs the first-run server's evidence asks for it by name.
    """
    directory = _artifact_directory(request)
    directory.mkdir(parents=True, exist_ok=True)

    try:
        page.screenshot(path=str(directory / f"{prefix}screenshot.png"), full_page=True)
    except Exception:
        # A crashed page must not hide the trace or the server log.
        pass

    context.tracing.stop(path=str(directory / f"{prefix}trace.zip"))
    (directory / f"{prefix}server.log").write_text(server.log_text, encoding="utf-8")


@pytest.fixture
def page(
    browser: Browser,
    browser_context_args: dict,
    live_server: LiveServer,
    request: pytest.FixtureRequest,
) -> Iterator[Page]:
    """A fresh page per test, traced for the whole test.

    The trace is kept only when the test fails, together with a screenshot and
    the server log; a green run writes nothing.

    These files are `live_server`'s, unprefixed. A test that also drives
    `first_run_page` gets a second set behind a `first-run-` prefix rather than
    having these overwritten, so both servers' evidence survives the failure.
    """
    context = browser.new_context(**browser_context_args)
    context.tracing.start(screenshots=True, snapshots=True, sources=True)
    # The browser context shares the session the harness logged in with, so
    # every test's `page.goto` is already authenticated and no test performs a
    # login step of its own. Gate-behaviour tests that need an unauthenticated
    # visitor build their own context (see tests/test_identity.py).
    #
    # A missing cookie means the harness failed to authenticate the suite, which
    # would silently turn every browser test into an anonymous one. Fail loudly
    # here instead of letting a whole slice degrade to redirects to /login.
    if live_server.session_cookie is None:
        raise SeedError(
            "the harness has no session cookie: `live_server` did not log in, so every "
            "browser test would silently run unauthenticated"
        )
    name, value = live_server.session_cookie
    context.add_cookies(
        [
            {
                "name": name,
                "value": value,
                "domain": "127.0.0.1",
                "path": "/",
                "httpOnly": True,
                "sameSite": "Lax",
            }
        ]
    )
    page = context.new_page()
    try:
        yield page
    finally:
        if _test_failed(request):
            _write_failure_artifacts(request, page, context, live_server)
        else:
            context.tracing.stop()
        context.close()


@pytest.fixture
def first_run_page(
    browser: Browser,
    browser_context_args: dict,
    first_run_server: LiveServer,
    request: pytest.FixtureRequest,
) -> Iterator[Page]:
    """A page on the pending-install server, whose context carries no session.

    The opposite of `page`, and the honest way to see a brand-new installation:
    `first_run_server` has no administrator to log in as and no session to
    share, so there is nothing to inject and nothing to strip. It is the context
    the gate-behaviour tests in tests/test_identity.py build by hand, for the
    same reason and against a server whose setup is already complete.

    Traced and artifacted exactly like `page`, because a test that cannot show
    what it saw is worth much less than one that can. The files carry a
    `first-run-` prefix because a test may hold this context *and* the shared
    `page` - the visual baseline does - and both finalize into the same per-test
    directory, so an unprefixed name would be whichever server was torn down
    last. The log written here is `first_run_server`'s own, which is the one that
    explains a `/setup` failure.
    """
    context = browser.new_context(**browser_context_args)
    context.tracing.start(screenshots=True, snapshots=True, sources=True)
    page = context.new_page()
    try:
        yield page
    finally:
        if _test_failed(request):
            _write_failure_artifacts(
                request, page, context, first_run_server, prefix="first-run-"
            )
        else:
            context.tracing.stop()
        context.close()
