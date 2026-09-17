"""Seed data for the browser suite through the application's HTTP API.

Every helper here calls an HTTP endpoint the application exposes, so the seed
cannot drift from the interface and a broken endpoint fails the seed loudly
instead of being papered over with direct SQL. That includes the routes the
browser itself calls and the documented JSON routes next to them (barcodes, for
instance, are added through ``POST /api/products/{id}/barcodes``). Nothing here
touches the development database: the server the client points at runs on a
throwaway file.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from decimal import Decimal
from typing import Any

# A seed request is a local call; ten seconds is far more than enough and keeps
# a wedged server from hanging the suite without a message.
_REQUEST_TIMEOUT_SECONDS = 10.0


class SeedError(RuntimeError):
    """A seed request failed; the endpoint is broken and the suite must say so."""


class ApiClient:
    """Minimal JSON/form client over the running application's HTTP API."""

    def __init__(self, base_url: str) -> None:
        self.base_url = base_url.rstrip("/")

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
    """Create an account and allow exactly `methods` on it.

    The account starts with an empty allowlist (that is the API contract), so
    the test that wants a working account must say which methods it accepts.
    """
    created = api.post_json("/api/accounts", {"name": name})
    account_id = int(created["id"])

    catalog = api.get_json(f"/api/accounts/{account_id}/payment-methods")
    method_ids = {method["name"]: method["id"] for method in catalog["methods"]}
    missing = [method for method in methods if method not in method_ids]
    if missing:
        raise SeedError(f"payment methods not in the catalog: {missing}")
    allowed = [method_ids[method] for method in methods]
    api.put_json(f"/api/accounts/{account_id}/payment-methods", {"method_ids": allowed})
    # Assert the effect, not merely the 2xx: re-read the allowlist. The allowlist
    # gates every payment in E2 and E3, so a silent no-op must fail here.
    confirmed = api.get_json(f"/api/accounts/{account_id}/payment-methods")
    confirmed_ids = sorted(int(method_id) for method_id in confirmed["allowed_method_ids"])
    if confirmed_ids != sorted(allowed):
        raise SeedError(
            f"account {account_id} allowlist is {confirmed_ids}, expected {sorted(allowed)}"
        )
    return account_id


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
) -> dict[str, Any]:
    """Create a tracked product, optionally with a barcode and opening stock."""
    product = api.post_json(
        "/api/products",
        {
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
        },
    )
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
    credit_limit: str | None = None,
    payment_days: int | None = None,
) -> int:
    created = api.post_json(
        "/api/customers",
        {
            "name": name,
            "phone": None,
            "address": None,
            "tax_id": None,
            "notes": None,
            "credit_limit": credit_limit,
            "payment_days": payment_days,
        },
    )
    # Customer creation answers with the created customer plus any name matches
    # (the duplicate-name warning), so the id lives under `customer`.
    return int(created["customer"]["id"])


def create_supplier(api: ApiClient, name: str, *, phone: str = "555-0100") -> int:
    supplier = api.post_json("/api/suppliers", {"name": name, "phone": phone, "notes": None})
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
) -> int:
    detail = api.post_json(
        "/api/sales",
        {
            "customer_id": customer_id,
            "payment_type": payment_type,
            "sale_date": sale_date,
            "due_date": None,
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


def create_purchase_draft(
    api: ApiClient,
    supplier_id: int,
    *,
    payment_type: str = "Cash",
    purchase_date: str = "2024-05-02",
) -> int:
    detail = api.post_json(
        "/api/purchases",
        {
            "supplier_id": supplier_id,
            "payment_type": payment_type,
            "purchase_date": purchase_date,
            "due_date": None,
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
