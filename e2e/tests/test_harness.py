"""Slice E1: the harness itself, proven end to end.

This module keeps only what proves the harness works: the application starts, the
dashboard and a record page render, the isolated server stops and releases its
port, a failure writes openable evidence, and the seed helpers refuse a silent
no-op. The picker, filter and confirmation flows are user behaviour and live in
their own modules (slices E2 and E3), so this file stays about the harness.
"""

from __future__ import annotations

import json
import os
import re
import threading
import zipfile
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
from playwright.sync_api import Page, expect

from conftest import ARTIFACTS_ROOT
from helpers import (
    ApiClient,
    SeedError,
    add_purchase_line,
    add_sale_line,
    create_account_with_methods,
    create_product,
    record_supplier_cost,
    seed_harness_data,
)


def test_dashboard_and_record_page_render(page: Page, api: ApiClient) -> None:
    """The seeded shop boots, renders the dashboard and opens a draft's page.

    This is the harness's own smoke test: it proves the seeded data reaches the
    interface and the record page renders its picker. The picker's behaviour is
    asserted where it belongs, in ``test_picker.py``.
    """
    data = seed_harness_data(api)

    page.goto(f"{api.base_url}/")
    expect(page).to_have_title(re.compile("Roya"))
    expect(page.locator("#total-balance")).to_be_visible()

    page.goto(f"{api.base_url}/sales/{data.sale_id}")
    expect(page.locator("#product-picker")).to_be_visible()
    # The seed's draft line really renders, so a later "the interaction added a
    # line" assertion cannot pass on an empty record.
    expect(page.locator("#sale-record-money table tbody tr")).to_have_count(1)
    detail = api.get_json(f"/api/sales/{data.sale_id}")
    assert [line["product_id"] for line in detail["lines"]] == [
        data.sale_line_product_id
    ], detail["lines"]


def test_server_stops_and_releases_the_port(live_server) -> None:
    """Stopping the server gives the port back instead of leaking it."""
    live_server.stop()
    assert live_server.wait_until_released()


@pytest.mark.skipif(
    os.environ.get("ROYA_E2E_ARTIFACT_PROBE") != "1",
    reason="opt-in probe: set ROYA_E2E_ARTIFACT_PROBE=1 to prove failure artifacts exist",
)
def test_failure_artifacts_are_written(page: Page, live_server) -> None:
    """Deliberately fails so the trace, screenshot and server log are written.

    It is skipped by default; the README documents how to run it and open the
    trace it leaves behind.
    """
    page.goto(f"{live_server.url}/")
    expect(page.locator("#total-balance")).to_be_visible()
    pytest.fail("deliberate failure: the harness must leave openable evidence")


@pytest.mark.skipif(
    os.environ.get("ROYA_E2E_ARTIFACT_PROBE") != "1",
    reason="opt-in probe: checked after the deliberate failure above has run",
)
def test_failure_artifacts_are_openable() -> None:
    """The artifacts written by the failing probe form a valid Playwright trace."""
    # The probe is parametrized by browser, so its directory name carries the
    # browser suffix; pick the most recent probe directory.
    directories = [
        path
        for path in ARTIFACTS_ROOT.glob("test_failure_artifacts_are_written*")
        if path.is_dir()
    ]
    assert directories, f"no artifact directory under {ARTIFACTS_ROOT}"
    directory = max(directories, key=lambda path: path.stat().st_mtime)
    trace = directory / "trace.zip"
    assert (directory / "screenshot.png").is_file(), f"no screenshot in {directory}"
    assert (directory / "server.log").is_file(), f"no server log in {directory}"
    assert trace.is_file(), f"no trace at {trace}"
    with zipfile.ZipFile(trace) as archive:
        names = archive.namelist()
    assert "trace.trace" in names, f"trace has no trace.trace: {names}"
    assert any(name.endswith("trace.network") for name in names), names


def test_throwaway_guard_rejects_a_server_on_another_database(
    roya_binary, tmp_path
) -> None:
    """A server started against the wrong database must fail the isolation guard.

    The real binary runs against a scratch file, but the guard is told the
    expected throwaway path is a different scratch file, so the server's own log
    cannot name it and the guard must raise. That is the exact shape of a
    misconfigured ``DATABASE_URL`` pointed at the wrong database.
    """
    import subprocess

    from conftest import (
        REPO_ROOT,
        LiveServer,
        _assert_throwaway_database,
        _free_port,
        _wait_until_ready,
    )

    port = _free_port()
    actual_db = tmp_path / "actual.db"
    expected_db = tmp_path / "expected.db"
    log_path = tmp_path / "wrong-server.log"
    environment = os.environ.copy()
    environment.update(
        {"DATABASE_URL": f"sqlite://{actual_db}", "PORT": str(port), "RUST_LOG": "info"}
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
            db_path=expected_db,
            log_path=log_path,
            process=process,
        )
        try:
            _wait_until_ready(server)
            with pytest.raises(RuntimeError, match="did not name the throwaway database"):
                _assert_throwaway_database(server, dev_before=None)
        finally:
            server.stop()


class _StubHandler(BaseHTTPRequestHandler):
    """Answers every request with a canned JSON body and status 200."""

    def _respond(self) -> None:
        payload = self.server.responses.get((self.command, self.path), {})
        body = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = _respond
    do_POST = _respond
    do_PUT = _respond

    def log_message(self, *args) -> None:  # keep the test output quiet
        pass


@contextmanager
def _stubbed_api(responses: dict[tuple[str, str], object]):
    """An ApiClient pointed at a stub that 2xx's every write without storing it."""
    server = ThreadingHTTPServer(("127.0.0.1", 0), _StubHandler)
    server.responses = responses
    thread = threading.Thread(
        target=lambda: server.serve_forever(poll_interval=0.05), daemon=True
    )
    thread.start()
    try:
        yield ApiClient(f"http://127.0.0.1:{server.server_address[1]}")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def test_seed_helpers_reject_a_2xx_that_moved_nothing() -> None:
    """A silent 2xx no-op must fail the seed, not surface three tests later.

    Each stub answers 2xx for the write but its read-back still shows the write
    missing, which is exactly the failure a status check cannot see.
    """
    with _stubbed_api(
        {
            ("POST", "/api/accounts"): {"id": 7},
            ("GET", "/api/accounts/7/payment-methods"): {
                "methods": [{"name": "Cash", "id": 1}],
                "allowed_method_ids": [],
            },
        }
    ) as api:
        with pytest.raises(SeedError, match="allowlist"):
            create_account_with_methods(api, "Caja", ("Cash",))

    with _stubbed_api(
        {
            ("POST", "/api/products"): {"id": 5},
            ("GET", "/api/products/5/stock"): {"stock": "0"},
        }
    ) as api:
        with pytest.raises(SeedError, match="stock"):
            create_product(api, sku="S", name="S", stock="5")

    with _stubbed_api(
        {
            ("POST", "/api/products"): {"id": 5},
            ("GET", "/api/products/5/barcodes"): {"barcodes": []},
        }
    ) as api:
        with pytest.raises(SeedError, match="barcode"):
            create_product(api, sku="S", name="S", barcode="779")

    with _stubbed_api(
        {
            ("POST", "/api/sales/3/lines"): {},
            ("GET", "/api/sales/3"): {"lines": []},
        }
    ) as api:
        with pytest.raises(SeedError, match="sale 3"):
            add_sale_line(api, 3, product_id=5, qty="1")

    with _stubbed_api(
        {
            ("POST", "/api/purchases/4/lines"): {},
            ("GET", "/api/purchases/4"): {"lines": []},
        }
    ) as api:
        with pytest.raises(SeedError, match="purchase 4"):
            add_purchase_line(api, 4, product_id=5, qty="1")

    with _stubbed_api(
        {
            ("POST", "/api/product-supplier-costs"): {},
            ("GET", "/api/product-supplier-costs?product_id=5"): {"costs": []},
        }
    ) as api:
        with pytest.raises(SeedError, match="supplier cost"):
            record_supplier_cost(api, 5, 6, cost="9.50")
