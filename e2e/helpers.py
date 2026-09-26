"""Seed data for the browser suite through the application's HTTP API.

Every helper here calls an HTTP endpoint the application exposes, so the seed
cannot drift from the interface and a broken endpoint fails the seed loudly
instead of being papered over with direct SQL. That includes the routes the
browser itself calls and the documented JSON routes next to them (barcodes, for
instance, are added through ``POST /api/products/{id}/barcodes``). Nothing here
touches the development database: the server the client points at runs on a
throwaway file.

The two deliberate exceptions are direct writes to the throwaway database, and
only there — never to the development database the harness guards:

- ``expire_session_in_database``: the AC22 session-expiry case needs the session
  invalid server-side while the browser still holds its cookie, and expiring the
  row is not an action the interface offers (logout revokes, it does not expire).
- ``reopen_first_run_setup_in_database``: the first-run wizard is one-time, so a
  server that has already completed setup answers ``/setup`` with a redirect.
  Removing the configuration row returns the throwaway server to the state a
  brand-new installation is in, which is the only way this suite can render the
  wizard without a second server lifecycle.

Both write to the file the spawned server already owns.
"""

from __future__ import annotations

import base64
import hashlib
import json
import sqlite3
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from decimal import Decimal
from pathlib import Path
from typing import Any

# A seed request is a local call; ten seconds is far more than enough and keeps
# a wedged server from hanging the suite without a message.
_REQUEST_TIMEOUT_SECONDS = 10.0

# Fixed first-run setup values for every fresh throwaway server. The application
# no longer bootstraps an administrator during startup, so the E2E harness must
# exercise the real setup form before it can log in. The browser baseline is
# en-US: its server-rendered money uses dot-decimal input and the USD code.
# Locale-specific parsing remains covered by the Rust localization tests.
E2E_ADMIN_USERNAME = "admin"
E2E_ADMIN_PASSWORD = "roya-e2e-fixed-password"
E2E_SETUP_FORM = {
    "business_name": "Roya E2E",
    "default_locale_code": "en-US",
    "currency_code": "USD",
    "timezone": "UTC",
    "username": E2E_ADMIN_USERNAME,
    "display_name": "Roya E2E Administrator",
    "password": E2E_ADMIN_PASSWORD,
}

# Browser expectations follow the configured business language. Canonical form
# values, API enum values, and test seed names never come from this map.
E2E_LANGUAGE = E2E_SETUP_FORM["default_locale_code"].split("-", 1)[0]
_E2E_COPY = {
    "en": {
        "username": "Username",
        "password": "Password",
        "current_password": "Current password",
        "new_password": "New password",
        "confirm_password": "Confirm new password",
        "sign_in": "Sign in",
        "save_password": "Save password",
        "confined": "Your session is restricted",
        "permission_required": "Permission",
        "delete_draft_impact": "The draft and its 1 line are deleted",
        "never_confirmed": "It was never confirmed",
        "sale_return_stock": "Stock for",
        "refund_account": "is refunded to",
        "no_undo": "This action cannot be undone",
        "cancelled": "Cancelled",
        "draft_ref": "Draft #",
        "annul": "Cancel",
        "annul_failed": "Cancel failed",
        "refresh": "↻ Refresh",
        "new_role": "New role",
        "save_permissions": "Save permissions",
        "edit_role": "Edit role and permissions",
        "create_role": "Create role",
        "save_roles": "Save roles",
        "new_user": "New user",
        "create_user": "Create user",
        "must_change": "must change password",
        "assign_roles": "Assign roles",
        "new_purchase": "New purchase",
        "create_draft": "Create Draft",
        "add_line": "Add line",
        "no_products": "No products match",
        "searching": "Searching",
        "two_matches": "2 matches.",
        "settings": "Business settings",
        "save_settings": "Save settings",
        "settings_saved": "Settings saved",
        # The Taxes tab (T3). `taxes_tab` is the tab-strip label, distinct from
        # the business tab's heading, so the two tabs cannot be told apart by a
        # copy collision in a test assertion.
        "taxes_tab": "Taxes",
        "create_tax": "Create tax",
        "delete_tax": "Delete tax",
        "delete_tax_label": "Delete tax {code}",
        "deactivate": "Deactivate",
        "activate": "Activate",
        "delete_confirm_title": "Delete this tax permanently?",
        "delete_confirm_submit": "Yes, delete it",
        "delete_blocked_by_products": "still linked to products",
        "delete_blocked_by_history": "already recorded this tax",
        "deactivate_instead": "Deactivate it instead",
        "delete_confirmation_required": "Confirm the deletion before it can run.",
        "products_linked": "Products still linked: {count}",
        "document_lines": "Recorded document lines: {count}",
        "save": "Save",
    },
    "es": {
        "username": "Usuario",
        "password": "Contraseña",
        "current_password": "Contraseña actual",
        "new_password": "Nueva contraseña",
        "confirm_password": "Confirmar nueva contraseña",
        "sign_in": "Iniciar sesión",
        "save_password": "Guardar contraseña",
        "confined": "Su sesión está restringida",
        "permission_required": "Se necesita el permiso",
        "delete_draft_impact": "Se eliminan el borrador y su 1 línea",
        "never_confirmed": "Nunca se confirmó",
        "sale_return_stock": "Se devuelve el stock de",
        "refund_account": "Se reembolsa",
        "no_undo": "Esta acción no se puede deshacer",
        "cancelled": "Anulada",
        "draft_ref": "Borrador n.º",
        "annul": "Anular",
        "annul_failed": "Anular falló",
        "refresh": "↻ Actualizar",
        "new_role": "Nuevo rol",
        "save_permissions": "Guardar permisos",
        "edit_role": "Editar rol y permisos",
        "create_role": "Crear rol",
        "save_roles": "Guardar roles",
        "new_user": "Nuevo usuario",
        "create_user": "Crear usuario",
        "must_change": "debe cambiar la contraseña",
        "assign_roles": "Asignar roles",
        "new_purchase": "Nueva compra",
        "create_draft": "Crear borrador",
        "add_line": "Agregar línea",
        "no_products": "Ningún producto coincide",
        "searching": "Buscando",
        "two_matches": "2 coincidencias.",
        "settings": "Configuración del negocio",
        "save_settings": "Guardar configuración",
        "settings_saved": "Configuración guardada",
        "taxes_tab": "Impuestos",
        "create_tax": "Crear impuesto",
        "delete_tax": "Eliminar impuesto",
        "delete_tax_label": "Eliminar impuesto {code}",
        "deactivate": "Desactivar",
        "activate": "Activar",
        "delete_confirm_title": "¿Eliminar este impuesto definitivamente?",
        "delete_confirm_submit": "Sí, eliminarlo",
        "delete_blocked_by_products": "sigue vinculado a productos",
        "delete_blocked_by_history": "ya registró este impuesto",
        "deactivate_instead": "Desactivalo en su lugar",
        "delete_confirmation_required": "Confirmá la eliminación antes de ejecutarla.",
        "products_linked": "Productos aún vinculados: {count}",
        "document_lines": "Líneas de documento registradas: {count}",
        "save": "Guardar",
    },
}


def e2e_copy(key: str) -> str:
    """Return presentation copy for the locale configured by this harness."""
    return _E2E_COPY.get(E2E_LANGUAGE, _E2E_COPY["en"])[key]



class SeedError(RuntimeError):
    """A seed request failed; the endpoint is broken and the suite must say so."""


# The timestamp form the application writes into SQLite (src/db.rs): ISO with a
# `T`, milliseconds and a trailing `Z`, so the SQL comparison
# `expires_at > :now` sees a value that is unambiguously in the past.
_EXPIRED_SQLITE_TIMESTAMP = "1970-01-01T00:00:00.000Z"


def expire_session_in_database(db_path: Path, token: str) -> None:
    """Expire the session row the raw cookie token names, in the throwaway db.

    AC22's session-expiry case must be about *expiry*, not logout: the row
    stays, the cookie stays in the browser, and the server alone must decide
    the session is dead. The digest is the same sha256/base64url-no-pad form
    the application stores (``security/session.rs``), computed from the token
    the browser actually holds, so the row updated here is the row the next
    request resolves.
    """
    digest = base64.urlsafe_b64encode(hashlib.sha256(token.encode("utf-8")).digest())
    token_hash = digest.decode("ascii").rstrip("=")
    connection = sqlite3.connect(str(db_path), timeout=5.0)
    try:
        connection.execute("PRAGMA busy_timeout = 5000")
        cursor = connection.execute(
            "UPDATE sessions SET expires_at = ? "
            "WHERE token_hash = ? AND revoked_at IS NULL",
            (_EXPIRED_SQLITE_TIMESTAMP, token_hash),
        )
        connection.commit()
        if cursor.rowcount != 1:
            raise SeedError(
                f"expected to expire exactly one live session row, moved {cursor.rowcount}; "
                "the token digest did not match a live row"
            )
    finally:
        connection.close()


def reopen_first_run_setup_in_database(db_path: Path) -> None:
    """Remove the singleton business configuration row, so `/setup` renders again.

    The first-run wizard is one-time: once `business_settings` holds its row the
    server answers `GET /setup` with a redirect to `/login`, so a harness that
    has already completed setup can never reach the page. Deleting the row puts
    the throwaway server back in the state a brand-new installation is in, and
    the wizard renders for real: the same route, the same template, the same
    committed stylesheet, read by the same browser.

    There is deliberately no fresh-database harness for this. The alternative —
    a second spawned server that skips `setup_fresh_server` — is a per-capture
    server lifecycle, and this suite's `live_server` fixture is the one place
    that knows how to prove a server opened the throwaway file and not
    `roya.db`. Reusing the fixture's own database keeps that proof intact.

    It writes to the throwaway database only, exactly as
    `expire_session_in_database` does, and never to the development database
    the harness guards. Nothing is left behind: the fixture's database is
    deleted with its temporary directory.
    """
    connection = sqlite3.connect(str(db_path), timeout=5.0)
    try:
        connection.execute("PRAGMA busy_timeout = 5000")
        cursor = connection.execute("DELETE FROM business_settings WHERE id = 1")
        connection.commit()
        if cursor.rowcount != 1:
            raise SeedError(
                "expected to remove exactly one business configuration row, removed "
                f"{cursor.rowcount}; the throwaway server was not in the seeded state"
            )
    finally:
        connection.close()


class ApiClient:
    """Minimal JSON/form client over the running application's HTTP API.

    Deliberately no cookie jar: the client carries the one session cookie
    ``POST /api/sessions`` sets and sends it on every request. The suite logs
    in once per server (see ``conftest``) and hands the cookie pair here, so
    seeding rides the same session the browser tests use.
    """

    def __init__(self, base_url: str, session_cookie: tuple[str, str] | None = None) -> None:
        self.base_url = base_url.rstrip("/")
        # (name, value) of the session cookie, or None while anonymous.
        self.session_cookie = session_cookie

    def login(self, username: str, password: str) -> tuple[str, str]:
        """Log in through ``POST /api/sessions`` and keep the session cookie.

        The endpoint answers ``204`` with a session ``Set-Cookie`` on success
        and a bare ``401`` without one on failure, so the header is the whole
        verdict: a refused login raises and leaves any stored cookie untouched.
        """
        request = urllib.request.Request(
            f"{self.base_url}/api/sessions",
            data=json.dumps({"username": username, "password": password}).encode(
                "utf-8"
            ),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=_REQUEST_TIMEOUT_SECONDS) as response:
                raw = response.headers.get("Set-Cookie")
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", errors="replace")
            raise SeedError(
                f"POST /api/sessions -> HTTP {error.code}: {detail}"
            ) from error
        except urllib.error.URLError as error:
            raise SeedError(f"POST /api/sessions failed: {error.reason}") from error
        if not raw:
            raise SeedError("POST /api/sessions answered without a Set-Cookie header")
        # The cookie is one `name=value` pair followed by flags, none of which
        # belong on the wire we send back.
        name, value = raw.split(";", 1)[0].split("=", 1)
        self.session_cookie = (name.strip(), value.strip())
        return self.session_cookie

    def get_json(self, path: str) -> Any:
        return self._request("GET", path, accept="application/json")

    def post_json(self, path: str, payload: dict[str, Any]) -> Any:
        body = json.dumps(payload).encode("utf-8")
        return self._request(
            "POST",
            path,
            body=body,
            content_type="application/json",
            accept="application/json",
        )

    def put_json(self, path: str, payload: dict[str, Any]) -> Any:
        body = json.dumps(payload).encode("utf-8")
        return self._request(
            "PUT",
            path,
            body=body,
            content_type="application/json",
            accept="application/json",
        )

    def post_form(self, path: str, fields: dict[str, Any]) -> Any:
        body = urllib.parse.urlencode(fields).encode("utf-8")
        return self._request(
            "POST",
            path,
            body=body,
            content_type="application/x-www-form-urlencoded",
        )

    def _request(
        self,
        method: str,
        path: str,
        *,
        body: bytes | None = None,
        content_type: str | None = None,
        accept: str | None = None,
    ) -> Any:
        headers: dict[str, str] = {}
        if self.session_cookie is not None:
            name, value = self.session_cookie
            headers["Cookie"] = f"{name}={value}"
        if content_type is not None:
            headers["Content-Type"] = content_type
        if accept is not None:
            headers["Accept"] = accept
        request = urllib.request.Request(
            f"{self.base_url}{path}", data=body, headers=headers, method=method
        )
        try:
            with urllib.request.urlopen(request, timeout=_REQUEST_TIMEOUT_SECONDS) as response:
                text = response.read().decode("utf-8")
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", errors="replace")
            raise SeedError(f"{method} {path} -> HTTP {error.code}: {detail}") from error
        except urllib.error.URLError as error:
            raise SeedError(f"{method} {path} failed: {error.reason}") from error

        if not text.strip():
            return None
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            # Form endpoints answer with HTML; the caller decides what to do.
            return {"raw": text}


def setup_fresh_server(api: ApiClient) -> None:
    """Complete the real first-run setup form for a fresh throwaway server.

    This intentionally uses ``ApiClient.post_form`` rather than writing to the
    database, so the harness proves the same public setup contract a person uses.
    """
    api.post_form("/setup", E2E_SETUP_FORM)


@dataclass(frozen=True)
class HarnessData:
    """Ids and values of one complete seeded shop, ready for a browser test."""

    account_id: int
    product_id: int
    product_name: str
    barcode: str
    customer_id: int
    supplier_id: int
    sale_id: int
    sale_line_product_id: int
    purchase_id: int


# ---------------------------------------------------------------------------
# Single-entity helpers
# ---------------------------------------------------------------------------


def create_account_with_methods(
    api: ApiClient, name: str = "Caja", methods: tuple[str, ...] = ("Cash",)
) -> int:
    """Create an account and attach exactly `methods` to it.

    The account starts owning nothing (that is the API contract), so the test
    that wants a working account must say which methods it accepts. Ticked
    methods are assigned (or duplicated when owned elsewhere); the read-back
    asserts the effect.
    """
    created = api.post_json("/api/accounts", {"name": name})
    account_id = int(created["id"])

    catalog = api.get_json("/api/payment-methods")["methods"]
    method_ids = {method["name"]: method["id"] for method in catalog}
    missing = [method for method in methods if method not in method_ids]
    if missing:
        raise SeedError(f"payment methods not in the catalog: {missing}")
    wanted = [method_ids[method] for method in methods]
    api.put_json(f"/api/accounts/{account_id}/payment-methods", {"method_ids": wanted})
    # Assert the effect, not merely the 2xx: re-read the account's methods. The
    # ownership gates every payment in E2 and E3, so a silent no-op must fail here.
    confirmed = api.get_json(f"/api/accounts/{account_id}/payment-methods")
    confirmed_ids = sorted(int(method_id) for method_id in confirmed["method_ids"])
    if sorted(wanted) != confirmed_ids and set(
        method["name"] for method in confirmed["methods"]
    ) != set(methods):
        raise SeedError(
            f"account {account_id} methods are {confirmed['methods']}, expected {sorted(methods)}"
        )
    return account_id


def payment_method_id(api: ApiClient, name: str = "Cash") -> int:
    """The global id of one payment method, so a seed can confirm a cash sale.

    Names repeat across accounts (one row per owner), so callers that need the
    row one account owns must read that account's methods instead.
    """
    catalog = api.get_json("/api/payment-methods")["methods"]
    for method in catalog:
        if method["name"] == name:
            return int(method["id"])
    raise SeedError(f"payment method {name!r} is not in the catalog: {catalog}")


def account_method_id(api: ApiClient, account_id: int, name: str = "Cash") -> int:
    """The id of one method the account owns.

    The same name can exist on several accounts (one row per owner), so a seed
    that must pay through THIS account reads its own catalog instead of the
    global one. An unassigned method is rejected here because the pay/collect
    forms disable it: a test that selected one would fail on a rule, not a bug.
    """
    owned = api.get_json(f"/api/accounts/{account_id}/payment-methods")["methods"]
    for method in owned:
        if method["name"] == name:
            return int(method["id"])
    raise SeedError(f"account {account_id} owns no method named {name!r}: {owned}")


def fund_account(
    api: ApiClient, account_id: int, amount: str, *, date: str = "2024-05-01"
) -> None:
    """Post an Income the account needs before it can pay anyone.

    Paying a supplier posts an Expense, and overdraft is blocked by default, so
    an unfunded account would fail the handover on a rule unrelated to the test.
    The read-back asserts the balance really moved, not merely that the POST 2xx'd.
    """
    api.post_json(
        "/api/transactions",
        {
            "account_id": account_id,
            "type": "Income",
            "amount": amount,
            "description": "seed funding",
            "reference": None,
            "date": date,
        },
    )
    balance = Decimal(str(api.get_json(f"/api/accounts/{account_id}")["balance"]))
    if balance < Decimal(amount):
        raise SeedError(
            f"account {account_id} balance is {balance} after funding {amount}"
        )


def create_product(
    api: ApiClient,
    *,
    sku: str,
    name: str,
    sale_price: str = "25.00",
    cost_price: str = "10.00",
    barcode: str | None = None,
    stock: str | None = None,
    unit: str = "un",
    min_stock: str | None = None,
    max_stock: str | None = None,
    markup_pct: str | None = None,
) -> dict[str, Any]:
    """Create a tracked product, optionally with a barcode and opening stock.

    ``markup_pct`` rides the request body only when given: omitting the key is
    the request an existing caller makes, and the API distinguishes an absent
    markup (manual price) from any value, so the seed must not imply one it was
    not asked for.
    """
    payload = {
        "sku": sku,
        "name": name,
        "kind": "Product",
        "category_id": None,
        "unit": unit,
        "sale_price": sale_price,
        "cost_price": cost_price,
        "track_stock": True,
        "min_stock": min_stock,
        "max_stock": max_stock,
        "location": None,
        "notes": None,
    }
    if markup_pct is not None:
        payload["markup_pct"] = markup_pct
    product = api.post_json("/api/products", payload)
    product_id = int(product["id"])
    if barcode is not None:
        api.post_json(f"/api/products/{product_id}/barcodes", {"code": barcode})
        stored = api.get_json(f"/api/products/{product_id}/barcodes")["barcodes"]
        if not any(entry["code"] == barcode for entry in stored):
            raise SeedError(
                f"barcode {barcode!r} was not stored on product {product_id}"
            )
    if stock is not None:
        api.post_json(
            "/api/stock-movements",
            {
                "product_id": product_id,
                "qty": stock,
                "type": "In",
                "reason": "Initial",
                "date": "2024-05-01",
            },
        )
        # Stock is the second load-bearing seed step: without it, confirming a
        # sale or purchase deducts nothing and E2/E3 fail far from their cause.
        derived = Decimal(str(api.get_json(f"/api/products/{product_id}/stock")["stock"]))
        if derived != Decimal(stock):
            raise SeedError(
                f"product {product_id} stock is {derived}, expected {stock}"
            )
    return product


def create_customer(
    api: ApiClient,
    name: str,
    *,
    phone: str | None = None,
    credit_limit: str | None = None,
    due_days: int | None = None,
    address: str | None = None,
    notes: str | None = None,
) -> int:
    created = api.post_json(
        "/api/customers",
        {
            "name": name,
            "phone": phone,
            "address": address,
            "tax_id": None,
            "notes": notes,
            "credit_limit": credit_limit,
            "due_days": due_days,
        },
    )
    # Customer creation answers with the created customer plus any name matches
    # (the duplicate-name warning), so the id lives under `customer`.
    return int(created["customer"]["id"])


def create_supplier(
    api: ApiClient, name: str, *, phone: str = "555-0100", notes: str | None = None
) -> int:
    supplier = api.post_json(
        "/api/suppliers", {"name": name, "phone": phone, "notes": notes}
    )
    return int(supplier["id"])


def record_supplier_cost(
    api: ApiClient, product_id: int, supplier_id: int, *, cost: str = "9.50"
) -> None:
    api.post_json(
        "/api/product-supplier-costs",
        {
            "product_id": product_id,
            "supplier_id": supplier_id,
            "cost": cost,
            "date": "2024-05-01",
        },
    )
    stored = api.get_json(f"/api/product-supplier-costs?product_id={product_id}")["costs"]
    if not any(
        int(entry["supplier_id"]) == supplier_id
        and Decimal(str(entry["current_cost"])) == Decimal(cost)
        for entry in stored
    ):
        raise SeedError(
            f"supplier cost {cost} for product {product_id} and supplier "
            f"{supplier_id} was not stored"
        )


def create_sale_draft(
    api: ApiClient,
    customer_id: int,
    *,
    payment_type: str = "Cash",
    sale_date: str = "2024-05-02",
    due_date: str | None = None,
) -> int:
    detail = api.post_json(
        "/api/sales",
        {
            "customer_id": customer_id,
            "payment_type": payment_type,
            "sale_date": sale_date,
            "due_date": due_date,
            "receipt_no": None,
            "notes": None,
        },
    )
    return int(detail["sale"]["id"])


def add_sale_line(
    api: ApiClient,
    sale_id: int,
    product_id: int,
    *,
    qty: str = "1",
    unit_price: str | None = None,
) -> None:
    api.post_json(
        f"/api/sales/{sale_id}/lines",
        {"product_id": product_id, "qty": qty, "unit_price": unit_price},
    )
    detail = api.get_json(f"/api/sales/{sale_id}")
    if not any(
        int(line["product_id"]) == product_id
        and Decimal(str(line["qty"])) == Decimal(qty)
        for line in detail["lines"]
    ):
        raise SeedError(
            f"sale {sale_id} has no line for product {product_id} with qty {qty}"
        )


def confirm_sale(
    api: ApiClient, sale_id: int, *, method_id: int | None = None
) -> None:
    """Confirm a draft sale and read the effect back.

    A confirmed sale carries its assigned number and drives the filter and
    confirmation slices, so a silent transition failure must fail the seed here.
    A cash sale names the method; a credit sale omits it (the account is derived
    from the method, and a credit sale must not carry one).
    """
    api.post_json(
        f"/api/sales/{sale_id}/confirm",
        {"method_id": method_id},
    )
    status = api.get_json(f"/api/sales/{sale_id}")["sale"]["status"]
    if status != "Confirmed":
        raise SeedError(f"sale {sale_id} status is {status!r}, expected 'Confirmed'")


def create_confirmed_credit_sale(
    api: ApiClient,
    customer_id: int,
    product_id: int,
    *,
    qty: str = "1",
    unit_price: str = "10.00",
    sale_date: str = "2024-05-02",
    due_date: str = "2024-06-01",
) -> int:
    """A Confirmed credit sale with one line, so a customer drawer has a document.

    Credit needs a due date (the customer may have no term), and confirming
    assigns the sale number the drawer renders.
    """
    sale_id = create_sale_draft(
        api,
        customer_id,
        payment_type="Credit",
        sale_date=sale_date,
        due_date=due_date,
    )
    add_sale_line(api, sale_id, product_id, qty=qty, unit_price=unit_price)
    confirm_sale(api, sale_id)
    return sale_id


def cancel_sale(api: ApiClient, sale_id: int, *, reason: str | None = None) -> None:
    """Cancel a draft or confirmed sale and read the effect back."""
    api.post_json(f"/api/sales/{sale_id}/cancel", {"reason": reason})
    status = api.get_json(f"/api/sales/{sale_id}")["sale"]["status"]
    if status != "Cancelled":
        raise SeedError(f"sale {sale_id} status is {status!r}, expected 'Cancelled'")


def create_purchase_draft(
    api: ApiClient,
    supplier_id: int,
    *,
    payment_type: str = "Cash",
    purchase_date: str = "2024-05-02",
    due_date: str | None = None,
) -> int:
    detail = api.post_json(
        "/api/purchases",
        {
            "supplier_id": supplier_id,
            "payment_type": payment_type,
            "purchase_date": purchase_date,
            "due_date": due_date,
            "supplier_invoice_no": None,
            "notes": None,
        },
    )
    return int(detail["purchase"]["id"])


def add_purchase_line(
    api: ApiClient,
    purchase_id: int,
    product_id: int,
    *,
    qty: str = "3",
    unit_cost: str | None = None,
) -> None:
    api.post_json(
        f"/api/purchases/{purchase_id}/lines",
        {"product_id": product_id, "qty": qty, "unit_cost": unit_cost},
    )
    detail = api.get_json(f"/api/purchases/{purchase_id}")
    if not any(
        int(line["product_id"]) == product_id
        and Decimal(str(line["qty"])) == Decimal(qty)
        for line in detail["lines"]
    ):
        raise SeedError(
            f"purchase {purchase_id} has no line for product {product_id} with qty {qty}"
        )


def confirm_purchase(
    api: ApiClient, purchase_id: int, *, method_id: int | None = None
) -> None:
    """Confirm a draft purchase and read the effect back.

    A cash purchase names the method; a credit purchase omits it. The read-back
    catches a 2xx that left the status behind, which is exactly the silent no-op
    the drawer's outstanding balance would hide.
    """
    api.post_json(
        f"/api/purchases/{purchase_id}/confirm",
        {"method_id": method_id},
    )
    status = api.get_json(f"/api/purchases/{purchase_id}")["purchase"]["status"]
    if status != "Confirmed":
        raise SeedError(
            f"purchase {purchase_id} status is {status!r}, expected 'Confirmed'"
        )


def create_confirmed_credit_purchase(
    api: ApiClient,
    supplier_id: int,
    product_id: int,
    *,
    qty: str = "3",
    unit_cost: str = "10.00",
    purchase_date: str = "2024-05-02",
    due_date: str = "2024-06-01",
) -> int:
    """A Confirmed credit purchase with one line, so a supplier has a payable.

    Credit needs a due date, and confirming receives the stock; the drawer sums
    the line's due into the outstanding balance the supplier test reads.
    """
    purchase_id = create_purchase_draft(
        api,
        supplier_id,
        payment_type="Credit",
        purchase_date=purchase_date,
        due_date=due_date,
    )
    add_purchase_line(api, purchase_id, product_id, qty=qty, unit_cost=unit_cost)
    confirm_purchase(api, purchase_id)
    return purchase_id


# ---------------------------------------------------------------------------
# The dataset a browser test starts from
# ---------------------------------------------------------------------------


def seed_harness_data(api: ApiClient) -> HarnessData:
    """Seed an account, a product with barcode and stock, a customer, a supplier
    with a product cost, and one draft sale and purchase with lines."""
    account_id = create_account_with_methods(api, "Caja", ("Cash",))

    product = create_product(
        api,
        sku="HARNESS-WIDGET",
        name="Harness Widget",
        sale_price="25.00",
        cost_price="10.00",
        barcode="7791234567890",
        stock="5",
        min_stock="1",
        max_stock="50",
    )
    product_id = int(product["id"])

    customer_id = create_customer(api, "Harness Buyer")
    supplier_id = create_supplier(api, "Harness Supplier")
    record_supplier_cost(api, product_id, supplier_id, cost="9.50")

    # The draft sale already carries a line so the scan is a second, distinct
    # line: the test can then tell "the scan added a line" from "the seed did".
    spare = create_product(
        api,
        sku="HARNESS-SPARE",
        name="Harness Spare",
        sale_price="5.00",
        cost_price="2.00",
        stock="2",
        min_stock="1",
        max_stock="10",
    )
    spare_id = int(spare["id"])

    sale_id = create_sale_draft(api, customer_id)
    add_sale_line(api, sale_id, spare_id, qty="1")

    purchase_id = create_purchase_draft(api, supplier_id)
    add_purchase_line(api, purchase_id, product_id, qty="3")

    return HarnessData(
        account_id=account_id,
        product_id=product_id,
        product_name="Harness Widget",
        barcode="7791234567890",
        customer_id=customer_id,
        supplier_id=supplier_id,
        sale_id=sale_id,
        sale_line_product_id=spare_id,
        purchase_id=purchase_id,
    )


@dataclass(frozen=True)
class FilterDataset:
    """Three sales that make each list criterion discriminate on its own.

    One draft for ``Filter Alpha``, one confirmed sale for the same customer, and
    one cancelled sale for ``Filter Beta``; the dates are separate months so the
    range filter can isolate one of them.
    """

    draft_id: int
    confirmed_id: int
    cancelled_id: int
    confirmed_number: str


def seed_filter_data(api: ApiClient) -> FilterDataset:
    """Seed the small sales set the filter tests narrow."""
    account_id = create_account_with_methods(api, "Filter Caja", ("Cash",))
    method_id = payment_method_id(api, "Cash")
    product = create_product(
        api,
        sku="FILTER-WIDGET",
        name="Filter Widget",
        sale_price="10.00",
        cost_price="4.00",
        stock="20",
        min_stock="1",
        max_stock="100",
    )
    product_id = int(product["id"])

    alpha_id = create_customer(api, "Filter Alpha")
    beta_id = create_customer(api, "Filter Beta")

    draft_id = create_sale_draft(api, alpha_id, sale_date="2024-05-01")
    add_sale_line(api, draft_id, product_id, qty="1")

    confirmed_id = create_sale_draft(api, alpha_id, sale_date="2024-06-01")
    add_sale_line(api, confirmed_id, product_id, qty="1")
    confirm_sale(api, confirmed_id, method_id=method_id)
    confirmed_number = str(
        api.get_json(f"/api/sales/{confirmed_id}")["sale"]["sale_number"]
    )

    cancelled_id = create_sale_draft(api, beta_id, sale_date="2024-07-01")
    add_sale_line(api, cancelled_id, product_id, qty="1")
    cancel_sale(api, cancelled_id, reason="seeded cancellation")

    return FilterDataset(
        draft_id=draft_id,
        confirmed_id=confirmed_id,
        cancelled_id=cancelled_id,
        confirmed_number=confirmed_number,
    )
