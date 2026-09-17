"""Slice E2: destructive actions ask before they act.

Cancelling a sale and discarding a draft both carry a confirmation dialog. The two
outcomes are different behaviours and are tested separately: dismissing the dialog
must leave the record exactly as it was — asserted on the page *and* through the
API, because "nothing happened" is easy to claim and worth checking twice — and
accepting it must perform the cancellation and reflect it.

Each test seeds its own sale, so none depends on another's order or leftovers.
"""

from __future__ import annotations

from playwright.sync_api import Page, expect

from helpers import (
    ApiClient,
    HarnessData,
    confirm_sale,
    payment_method_id,
    seed_harness_data,
)


def _confirmed_sale(page: Page, api: ApiClient) -> HarnessData:
    """A seed sale confirmed through the API, open on its record page."""
    data = seed_harness_data(api)
    confirm_sale(
        api,
        data.sale_id,
        account_id=data.account_id,
        method_id=payment_method_id(api, data.account_id, "Cash"),
    )
    page.goto(f"{api.base_url}/sales/{data.sale_id}")
    expect(page.locator("#sale-record")).to_contain_text("Confirmed")
    return data


def _sale_status(api: ApiClient, sale_id: int) -> str:
    return api.get_json(f"/api/sales/{sale_id}")["sale"]["status"]


def _answer_next_dialog(page: Page, *, accept: bool) -> list[str]:
    """Answer the next dialog the moment it opens, and record what it asked.

    The confirmation is a native dialog raised inside the click handler, so the
    page's JavaScript is blocked until it is answered. A persistent handler is what
    lets the click finish; ``page.expect_event`` alone would deadlock the click.
    """
    seen: list[str] = []

    def handler(dialog) -> None:
        seen.append(dialog.message)
        if accept:
            dialog.accept()
        else:
            dialog.dismiss()

    page.on("dialog", handler)
    return seen


def test_dismissing_the_cancel_dialog_leaves_the_sale_confirmed(
    page: Page, api: ApiClient
) -> None:
    """The dialog appears, is dismissed, and nothing moves."""
    data = _confirmed_sale(page, api)
    seen = _answer_next_dialog(page, accept=False)

    page.locator("#cancel-sale button[type='submit']").click()

    assert seen and "Cancel this sale?" in seen[-1], seen
    # Still confirmed on the page, with the cancel control still offered, and
    # confirmed in the data the API returns.
    expect(page.locator("#sale-record")).to_contain_text("Confirmed")
    expect(page.locator("#cancel-sale")).to_be_visible()
    assert _sale_status(api, data.sale_id) == "Confirmed"


def test_accepting_the_cancel_dialog_cancels_the_sale(
    page: Page, api: ApiClient
) -> None:
    """The dialog appears, is accepted, and the sale is cancelled."""
    data = _confirmed_sale(page, api)
    seen = _answer_next_dialog(page, accept=True)

    page.locator("#cancel-sale button[type='submit']").click()

    assert seen and "Cancel this sale?" in seen[-1], seen
    expect(page.locator("#sale-record")).to_contain_text("Cancelled")
    # A cancelled sale is read-only: the cancel form is gone.
    expect(page.locator("#cancel-sale")).to_have_count(0)
    assert _sale_status(api, data.sale_id) == "Cancelled"


def test_dismissing_the_discard_dialog_leaves_the_draft(
    page: Page, api: ApiClient
) -> None:
    """Discarding a draft asks too, and dismissing it changes nothing."""
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")
    expect(page.locator("#discard-sale")).to_be_visible()
    seen = _answer_next_dialog(page, accept=False)

    page.locator("#discard-sale button[type='submit']").click()

    assert seen and "Discard this draft?" in seen[-1], seen
    expect(page.locator("#discard-sale")).to_be_visible()
    assert _sale_status(api, data.sale_id) == "Draft"


def test_accepting_the_discard_dialog_cancels_the_draft(
    page: Page, api: ApiClient
) -> None:
    """Accepting the discard performs the cancellation."""
    data = seed_harness_data(api)
    page.goto(f"{api.base_url}/sales/{data.sale_id}")
    seen = _answer_next_dialog(page, accept=True)

    page.locator("#discard-sale button[type='submit']").click()

    assert seen and "Discard this draft?" in seen[-1], seen
    expect(page.locator("#sale-record")).to_contain_text("Cancelled")
    expect(page.locator("#discard-sale")).to_have_count(0)
    assert _sale_status(api, data.sale_id) == "Cancelled"
