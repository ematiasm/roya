"""Post-setup business settings through the real authenticated browser flow."""

from __future__ import annotations

from playwright.sync_api import Page, expect

from conftest import LiveServer
from helpers import (
    E2E_LANGUAGE,
    ApiClient,
    add_purchase_line,
    create_product,
    create_purchase_draft,
    create_supplier,
    e2e_copy,
)


def test_admin_can_open_and_update_business_settings(
    page: Page, live_server: LiveServer
) -> None:
    """The protected administrator sees the sidebar entry and can save the form."""
    page.goto(f"{live_server.url}/")

    settings_link = page.locator('[data-nav="settings"]')
    expect(settings_link).to_be_visible()
    expect(settings_link).to_have_attribute("href", "/settings")
    settings_link.click()

    expect(page).to_have_url(f"{live_server.url}/settings")
    expect(page.get_by_role("heading", name=e2e_copy("settings"))).to_be_visible()
    expect(page.locator('[data-nav="settings"]')).to_have_attribute(
        "aria-current", "page"
    )
    expect(page.locator("#business_name")).to_have_value("Roya E2E")
    expect(page.locator("#default_locale_code")).to_have_value("en-US")
    expect(page.locator("#currency_code")).to_have_value("USD")
    expect(page.locator("#timezone")).to_have_value("UTC")
    expect(page.locator("#display_name_0")).to_have_value(
        "English (United States)" if E2E_LANGUAGE == "en" else "Inglés (Estados Unidos)"
    )

    page.locator("#business_name").fill("Roya Settings E2E")
    page.locator("#currency_code").select_option("EUR")
    page.locator("#timezone").fill("Europe/Madrid")
    page.locator("#display_name_0").fill("E2E locale")
    page.get_by_role("button", name=e2e_copy("save_settings")).click()

    expect(page).to_have_url(f"{live_server.url}/settings?saved=true")
    expect(page.locator("[data-notice='success']")).to_contain_text(
        e2e_copy("settings_saved")
    )
    expect(page.locator("#business_name")).to_have_value("Roya Settings E2E")
    expect(page.locator("#currency_code")).to_have_value("EUR")
    expect(page.locator("#timezone")).to_have_value("Europe/Madrid")
    expect(page.locator("#display_name_0")).to_have_value("E2E locale")


def _open_taxes_tab(page: Page, live_server: LiveServer) -> None:
    """Drive the tab strip the way an operator does: from the settings page."""
    page.goto(f"{live_server.url}/settings")
    taxes_tab = page.locator('[data-settings-tab="taxes"]')
    expect(taxes_tab).to_be_visible()
    taxes_tab.click()
    expect(page).to_have_url(f"{live_server.url}/settings?tab=taxes")
    expect(page.locator('[data-settings-tab="taxes"]')).to_have_attribute(
        "aria-current", "page"
    )
    expect(page.locator("#settings-tax-list")).to_be_visible()
    # The business form belongs to the other tab: the switch is a real switch.
    expect(page.locator("#business_name")).to_have_count(0)


def test_admin_manages_taxes_from_the_settings_taxes_tab(
    page: Page, live_server: LiveServer
) -> None:
    """Create, edit, deactivate, activate and delete a tax without leaving the tab."""
    _open_taxes_tab(page, live_server)
    catalogue = page.locator("#settings-tax-list")
    # The rate is written the way this locale writes a decimal; the server
    # stores the canonical value and reads it back through the same context.
    separator = "," if E2E_LANGUAGE == "es" else "."

    page.locator("#settings-new-tax-code").fill("IVA-E2E")
    page.locator("#settings-new-tax-name").fill("IVA E2E")
    page.locator("#settings-new-tax-rate").fill(f"21{separator}5")
    page.get_by_role("button", name=e2e_copy("create_tax")).click()

    # The code and the name are the row's field VALUES, not text: the
    # assertion has to read the control, not the container's text.
    row = catalogue.locator("form").filter(has=page.locator('input[value="IVA-E2E"]'))
    expect(row).to_have_count(1)
    expect(catalogue).to_contain_text(f"21{separator}5 %")

    # Edit the rate in place: the row is one form, so Save posts the row.
    row.locator('input[name="rate"]').fill(f"22{separator}25")
    row.get_by_role("button", name=e2e_copy("save")).click()
    expect(catalogue).to_contain_text(f"22{separator}25 %")

    # Lifecycle: deactivate, then bring it back, from the tab's own actions.
    catalogue.get_by_role("button", name=e2e_copy("deactivate")).click()
    expect(catalogue.get_by_role("button", name=e2e_copy("activate"))).to_be_visible()
    catalogue.get_by_role("button", name=e2e_copy("activate")).click()
    expect(catalogue.get_by_role("button", name=e2e_copy("deactivate"))).to_be_visible()

    # The hard delete is a two-step: the first click only asks for a
    # confirmation that names the tax and the references it still has.
    catalogue.get_by_role("button", name=e2e_copy("delete_tax_label").format(
        code="IVA-E2E"
    )).click()
    confirmation = page.locator("#settings-tax-delete")
    expect(confirmation).to_contain_text(e2e_copy("delete_confirm_title"))
    expect(confirmation).to_contain_text("IVA-E2E")
    expect(page.locator('input[name="confirm"]')).to_have_value("on")
    expect(catalogue.locator('input[value="IVA-E2E"]')).to_have_count(1)

    confirmation.get_by_role("button", name=e2e_copy("delete_confirm_submit")).click()
    expect(catalogue.locator('input[value="IVA-E2E"]')).to_have_count(0)


def test_tax_delete_is_refused_while_a_product_is_linked(
    page: Page, live_server: LiveServer, api: ApiClient
) -> None:
    """A linked product keeps the tax, and the refusal says what to do about it."""
    tax = api.post_json(
        "/api/taxes",
        {"code": "IVA-LINK", "name": "IVA linked", "rate": "21.00", "is_active": True},
    )
    product = create_product(
        api,
        sku="TAX-E2E-LINK",
        name="Linked tax product",
        min_stock="1",
        max_stock="100",
    )
    api.post_json(
        f"/api/products/{product['id']}/taxes", {"tax_id": int(tax["id"])}
    )

    _open_taxes_tab(page, live_server)
    catalogue = page.locator("#settings-tax-list")
    expect(catalogue.locator('input[value="IVA-LINK"]')).to_have_count(1)

    catalogue.get_by_role("button", name=e2e_copy("delete_tax_label").format(
        code="IVA-LINK"
    )).click()
    page.locator("#settings-tax-delete").get_by_role(
        "button", name=e2e_copy("delete_confirm_submit")
    ).click()

    expect(page.locator("[data-notice='error']")).to_contain_text(
        e2e_copy("delete_blocked_by_products")
    )
    for leak in ("FOREIGN KEY", "SQLITE", "sqlite"):
        expect(page.locator("[data-notice='error']")).not_to_contain_text(leak)
    expect(catalogue.locator('input[value="IVA-LINK"]')).to_have_count(1)


def test_delete_confirmation_reports_both_reference_counts(
    page: Page, live_server: LiveServer, api: ApiClient
) -> None:
    """The confirmation states the two counts that decide the delete, in the
    session's own language, instead of leaving the operator to guess."""
    tax = api.post_json(
        "/api/taxes",
        {"code": "IVA-COUNTS", "name": "IVA counts", "rate": "21.00", "is_active": True},
    )
    product = create_product(
        api,
        sku="TAX-E2E-COUNTS",
        name="Counted tax product",
        min_stock="1",
        max_stock="100",
    )
    api.post_json(f"/api/products/{product['id']}/taxes", {"tax_id": int(tax["id"])})

    _open_taxes_tab(page, live_server)
    page.locator("#settings-tax-list").get_by_role(
        "button", name=e2e_copy("delete_tax_label").format(code="IVA-COUNTS")
    ).click()

    panel = page.locator("#settings-tax-delete")
    expect(panel).to_contain_text(e2e_copy("products_linked").format(count=1))
    expect(panel).to_contain_text(e2e_copy("document_lines").format(count=0))
    # Nothing was deleted by asking: the tax is still in the catalogue.
    expect(page.locator('#settings-tax-list input[value="IVA-COUNTS"]')).to_have_count(1)


def test_tax_delete_is_refused_by_a_recorded_document_line(
    page: Page, live_server: LiveServer, api: ApiClient
) -> None:
    """A tax a document already froze is kept, and the refusal says why.

    The line is written through the real purchase flow, so the snapshot the
    refusal counts is one the application produced itself.
    """
    tax = api.post_json(
        "/api/taxes",
        {"code": "IVA-FROZEN", "name": "IVA frozen", "rate": "21.00", "is_active": True},
    )
    product = create_product(
        api,
        sku="TAX-E2E-FROZEN",
        name="Frozen tax product",
        min_stock="1",
        max_stock="100",
    )
    api.post_json(f"/api/products/{product['id']}/taxes", {"tax_id": int(tax["id"])})
    purchase = create_purchase_draft(api, create_supplier(api, "Frozen tax supplier"))
    add_purchase_line(api, purchase, int(product["id"]), qty="1")

    _open_taxes_tab(page, live_server)
    catalogue = page.locator("#settings-tax-list")
    catalogue.get_by_role(
        "button", name=e2e_copy("delete_tax_label").format(code="IVA-FROZEN")
    ).click()
    page.locator("#settings-tax-delete").get_by_role(
        "button", name=e2e_copy("delete_confirm_submit")
    ).click()

    notice = page.locator("[data-notice='error']")
    expect(notice).to_contain_text(e2e_copy("delete_blocked_by_history"))
    expect(notice).to_contain_text(e2e_copy("deactivate_instead"))
    for leak in ("FOREIGN KEY", "SQLITE", "sqlite", "no such table"):
        expect(notice).not_to_contain_text(leak)
    expect(catalogue.locator('input[value="IVA-FROZEN"]')).to_have_count(1)


def test_delete_without_the_confirmation_field_is_refused(
    page: Page, live_server: LiveServer, api: ApiClient
) -> None:
    """The two-step is enforced by the server, not only by the button.

    The request goes out through the browser context's own request API, so it
    carries the real session cookie and answers exactly what an operator whose
    confirmation step was skipped would have been shown.
    """
    tax = api.post_json(
        "/api/taxes",
        {"code": "IVA-NOCONF", "name": "IVA no confirm", "rate": "21.00", "is_active": True},
    )
    response = page.request.post(
        f"{live_server.url}/web/settings/taxes/delete",
        form={"id": str(int(tax["id"]))},
    )
    assert response.status == 400, response.text()
    assert e2e_copy("delete_confirmation_required") in response.text(), response.text()
    for leak in ("FOREIGN KEY", "SQLITE", "sqlite"):
        assert leak not in response.text(), response.text()

    _open_taxes_tab(page, live_server)
    expect(
        page.locator('#settings-tax-list input[value="IVA-NOCONF"]')
    ).to_have_count(1)
