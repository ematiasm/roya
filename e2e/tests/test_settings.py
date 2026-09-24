"""Post-setup business settings through the real authenticated browser flow."""

from __future__ import annotations

from playwright.sync_api import Page, expect

from conftest import LiveServer


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
    expect(page.get_by_role("heading", name="Configuración del negocio")).to_be_visible()
    expect(page.locator('[data-nav="settings"]')).to_have_attribute(
        "aria-current", "page"
    )
    expect(page.locator("#business_name")).to_have_value("Roya E2E")
    expect(page.locator("#default_locale_code")).to_have_value("en-US")
    expect(page.locator("#currency_code")).to_have_value("USD")
    expect(page.locator("#timezone")).to_have_value("UTC")
    expect(page.locator("#display_name_0")).to_have_value(
        "English (United States)"
    )

    page.locator("#business_name").fill("Roya Settings E2E")
    page.locator("#currency_code").fill("EUR")
    page.locator("#timezone").fill("Europe/Madrid")
    page.locator("#display_name_0").fill("English (E2E)")
    page.get_by_role("button", name="Guardar configuración").click()

    expect(page).to_have_url(f"{live_server.url}/settings?saved=true")
    expect(page.locator("[data-notice='success']")).to_contain_text(
        "Configuración guardada"
    )
    expect(page.locator("#business_name")).to_have_value("Roya Settings E2E")
    expect(page.locator("#currency_code")).to_have_value("EUR")
    expect(page.locator("#timezone")).to_have_value("Europe/Madrid")
    expect(page.locator("#display_name_0")).to_have_value("English (E2E)")
