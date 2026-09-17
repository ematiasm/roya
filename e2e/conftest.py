"""Fixtures for the roya browser suite (slice E1: the harness).

The suite drives the real binary against a throwaway SQLite file on a free
port. It never touches the development database, and it proves it: after the
server answers, its own log must name the throwaway file and the development
database must be unchanged. It never sleeps past a debounce, and a failing test
leaves a Playwright trace, a screenshot and the captured server log under
``e2e/.artifacts/<test name>/``.

The fixtures live at the suite root so ``tests/`` can stay a flat directory of
plain modules.
"""

from __future__ import annotations

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

from helpers import ApiClient

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

# The development database the harness must never open. Its state is captured
# before each spawn so the live server can be proven not to have written it.
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
    """Prove the live server opened the throwaway file, not another database.

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
            "the live server did not name the throwaway database in its own log; it "
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


@pytest.fixture
def live_server(roya_binary: Path, tmp_path: Path) -> Iterator[LiveServer]:
    """A real server on a free port, against a fresh throwaway database."""
    port = _free_port()
    db_path = tmp_path / "roya.db"
    log_path = tmp_path / "server.log"

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
def api(live_server: LiveServer) -> ApiClient:
    """HTTP client for seeding, on the same endpoints the browser uses."""
    return ApiClient(live_server.url)


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
) -> None:
    directory = _artifact_directory(request)
    directory.mkdir(parents=True, exist_ok=True)

    try:
        page.screenshot(path=str(directory / "screenshot.png"), full_page=True)
    except Exception:
        # A crashed page must not hide the trace or the server log.
        pass

    context.tracing.stop(path=str(directory / "trace.zip"))
    (directory / "server.log").write_text(server.log_text, encoding="utf-8")


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
    """
    context = browser.new_context(**browser_context_args)
    context.tracing.start(screenshots=True, snapshots=True, sources=True)
    page = context.new_page()
    try:
        yield page
    finally:
        if _test_failed(request):
            _write_failure_artifacts(request, page, context, live_server)
        else:
            context.tracing.stop()
        context.close()
