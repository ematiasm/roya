"""The visual-neutrality net for the component-token refactor.

`ui-component-tokens` moves styling out of `@layer base` and into component
classes. The whole point is that **no screen changes**, and that claim cannot be
checked by reading a diff: the classes are exactly what is supposed to change,
so a class diff proves nothing about what the operator sees.

So this records what the browser actually computes. For a fixed set of pages it
walks the DOM and, for every element, stores a fingerprint of its computed
style. The result is committed as a golden file and compared on every run.

Two deliberate choices:

- **Elements are keyed by DOM path, not by class.** The classes are the thing
  being rewritten; keying by them would make every entry look changed and the
  comparison useless. A path key says "the same element, in the same place".
- **The baseline is captured from the pre-refactor tree.** One captured
  afterwards would record whatever the refactor produced and prove nothing, so
  regeneration is behind an explicit environment variable that CI never sets.

Regenerate deliberately, and only when a change to the interface is intended:

    ROYA_VISUAL_BASELINE=write scripts/e2e.sh -k visual_baseline
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

from playwright.sync_api import Page

from helpers import ApiClient, HarnessData, seed_harness_data

BASELINE = Path(__file__).resolve().parent.parent / "visual-baseline.json"

# A fixed viewport so widths and heights are reproducible. The suite runs
# headless at 1280x720 by default; the desktop layout is what this protects.
VIEWPORT = {"width": 1440, "height": 900}

# What a person actually sees. Layout-affecting properties are included on
# purpose: a refactor that drops a `w-full` or a `gap` changes the screen even
# though no colour moved.
STYLE_PROPERTIES = [
    "color",
    "background-color",
    "border-top-width",
    "border-top-style",
    "border-top-color",
    "border-top-left-radius",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "margin-top",
    "margin-bottom",
    "font-size",
    "font-weight",
    "letter-spacing",
    "text-transform",
    "text-decoration-line",
    "display",
    "gap",
    "opacity",
    "cursor",
    "width",
    "height",
]

# Walks the whole body and fingerprints every element. The path is built from
# tag names and child indices only, so it survives a class rewrite.
_WALK = """
(props) => {
  const out = {};
  const walk = (el, path) => {
    const cs = getComputedStyle(el);
    const fp = {};
    for (const p of props) fp[p] = cs.getPropertyValue(p);
    out[path] = fp;
    let i = 0;
    for (const child of el.children) {
      walk(child, path + "/" + child.tagName.toLowerCase() + "[" + i + "]");
      i++;
    }
  };
  walk(document.body, "body");
  return out;
}
"""


def _pages(data: HarnessData) -> list[tuple[str, str]]:
    """The screens the component set touches, with a stable name each."""
    return [
        ("dashboard", "/"),
        ("products", "/products"),
        ("purchases", "/purchases"),
        ("purchase-record", f"/purchases/{data.purchase_id}"),
        ("sales", "/sales"),
        ("sale-record", f"/sales/{data.sale_id}"),
        ("customers", "/customers"),
        ("suppliers", "/suppliers"),
        ("documents", "/documents"),
        ("users", "/users"),
        ("roles", "/roles"),
    ]


def _fingerprint(page: Page) -> dict[str, Any]:
    return page.evaluate(_WALK, STYLE_PROPERTIES)


def test_visual_baseline(page: Page, api: ApiClient) -> None:
    """Every screen computes the same styles it did before the refactor."""
    data = seed_harness_data(api)
    page.set_viewport_size(VIEWPORT)

    current: dict[str, Any] = {}
    for name, path in _pages(data):
        page.goto(f"{api.base_url}{path}")
        page.wait_for_load_state("networkidle")
        current[name] = _fingerprint(page)

    # The drawers are the richest component surfaces in the app and they are
    # fragments, not pages, so they are reached by clicking. Snapshot the open
    # state, which is the only state an operator ever styles.
    for name, path, row in (
        ("products-drawer", "/products", f"#product-{data.product_id} button"),
        ("purchases-drawer", "/purchases", f"#purchase-{data.purchase_id}"),
    ):
        page.goto(f"{api.base_url}{path}")
        page.locator(row).first.click()
        page.wait_for_timeout(400)
        current[name] = _fingerprint(page)

    if os.environ.get("ROYA_VISUAL_BASELINE") == "write":
        BASELINE.write_text(json.dumps(current, sort_keys=True, separators=(",", ":")) + "\n")
        print(f"\nwrote {BASELINE} ({BASELINE.stat().st_size} bytes)", flush=True)
        return

    assert BASELINE.exists(), (
        f"no baseline at {BASELINE}. Capture it from the tree whose appearance "
        "you want to freeze: ROYA_VISUAL_BASELINE=write scripts/e2e.sh -k "
        "visual_baseline. A baseline captured AFTER the change you are trying to "
        "detect records that change and proves nothing."
    )
    expected = json.loads(BASELINE.read_text())

    # Report the first differences in a shape a person can act on, rather than
    # dumping two dictionaries at each other.
    problems: list[str] = []
    for name, _ in _pages(data):
        want, got = expected.get(name, {}), current.get(name, {})
        if want == got:
            continue
        for dom_path in sorted(set(want) | set(got)):
            if want.get(dom_path) == got.get(dom_path):
                continue
            if dom_path not in want:
                problems.append(f"{name}: element appeared at {dom_path}")
            elif dom_path not in got:
                problems.append(f"{name}: element gone from {dom_path}")
            else:
                for prop in STYLE_PROPERTIES:
                    a, b = want[dom_path].get(prop), got[dom_path].get(prop)
                    if a != b:
                        problems.append(
                            f"{name}: {dom_path}\n      {prop}: {a!r} -> {b!r}"
                        )
            if len(problems) > 40:
                problems.append("... (stopping after 40 differences)")
                break
        if len(problems) > 40:
            break

    assert not problems, (
        "the interface changed.\n"
        "  If that was not intended, the diff above is the defect.\n"
        "  If it WAS intended, regenerate deliberately and say so in the commit:\n"
        "      ROYA_VISUAL_BASELINE=write scripts/e2e.sh -k visual_baseline\n"
        "  A baseline regenerated to make a red test green is not evidence.\n\n  "
        + "\n  ".join(problems)
    )
