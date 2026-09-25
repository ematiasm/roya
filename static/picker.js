/*
 * The product picker island (redesign-interface N5; feature picker-island T3).
 *
 * One owner for the picker's client state. Before this module the widget's
 * state lived in four places — the DOM, base.html's focus machinery, the
 * Askama fragment and smuggled `hx-vals` — and that split was the direct
 * cause of the hidden-id defect (a pre-filled product id silently beating a
 * freshly typed name) and of the hand-written focus restoration in base.html.
 *
 * The island owns input + qty + price + results + busy + status inside one
 * `[data-picker]` container. One state object, one render function; every
 * visible thing is derived in render():
 *
 *   state = { query, matches, status, focusedProductId, selectedProductId }
 *   status: idle | searching | done | failed
 *
 * The add line itself is NOT reimplemented: the Askama shell keeps exactly
 * one htmx form (hx-post/hx-target/hx-select, the qty and price inputs and a
 * hidden `product_id`). Clicking a match sets that hidden input from island
 * state and calls `form.requestSubmit()`, so htmx keeps doing the swap, the
 * out-of-band fragments and the HX-Trigger events. The server's add-line
 * contract is untouched.
 *
 * Enter keeps posting the typed `product` text for the server to resolve by
 * name, SKU or barcode — decided in odd/tasks/picker-island.md. The two
 * paths no longer compete because the island owns product_id: any typed
 * change clears the selection, and while nothing is selected the hidden
 * input is disabled, so it never reaches the wire and the two paths cannot
 * compete the way markup-owned `hx-vals` did.
 *
 * Accessibility (reproduced, not redesigned): `#product-search-results` is
 * the one polite live region (`role="status"`, `aria-live="polite"`,
 * `aria-atomic="false"`); `#product-search-status` is its sr-only announced
 * text and the input's `aria-describedby` target; the visual list sits under
 * `aria-live="off"`; the visible busy cue is `aria-hidden="true"` and shown
 * through the htmx-indicator class pair in the stylesheet.
 */
(function () {
  'use strict';

  var DEBOUNCE_MS = 250;
  var SEARCH_URL = '/web/product-search.json';

  function mount(container) {
    if (container.dataset.pickerIsland) return;
    container.dataset.pickerIsland = 'true';

    var form = container.querySelector('form');
    var input = container.querySelector('#product-picker');
    var results = container.querySelector('#product-search-results');
    var busy = container.querySelector('#product-search-busy');
    var hidden = form ? form.querySelector('input[name="product_id"]') : null;
    if (!form || !input || !results || !busy || !hidden) return;

    // The calling context works in one price: `cost` for a purchase line,
    // `sale` (the default) for a sale line. The money strings arrive from
    // the server already in display form and are rendered verbatim — the
    // island never parses or reformats money, so the server keeps the one
    // formatting rule.
    var priceKind = container.dataset.priceKind === 'cost' ? 'cost' : 'sale';
    var messages = {
      idle: container.dataset.pickerStatusIdle,
      searching: container.dataset.pickerStatusSearching,
      failed: container.dataset.pickerStatusFailed,
      noResults: container.dataset.pickerNoResults,
      matchCountOne: container.dataset.pickerMatchCountOne,
      matchCountMany: container.dataset.pickerMatchCountMany,
      cost: container.dataset.pickerCostLabel,
      stock: container.dataset.pickerStockLabel
    };

    var state = {
      query: '',
      matches: [],
      status: 'idle',
      focusedProductId: null,
      selectedProductId: null
    };

    // The live region's one honest sentence, derived from status. "Searching…"
    // while in flight; the count when it lands; the empty-state message when
    // the query is non-empty and nothing matches; the failure state when the
    // request did not. No second region announces beside it.
    function interpolate(template, name, value) {
      return template.replace('{' + name + '}', value);
    }

    function noResultsMessage() {
      return interpolate(messages.noResults, 'query', state.query);
    }

    function statusMessage() {
      if (state.status === 'idle') return messages.idle;
      if (state.status === 'searching') return messages.searching;
      if (state.status === 'failed') return messages.failed;
      if (state.matches.length === 0) {
        return state.query === '' ? messages.idle : noResultsMessage();
      }
      var n = state.matches.length;
      var template = n === 1 ? messages.matchCountOne : messages.matchCountMany;
      return interpolate(template, 'count', String(n));
    }

    function statusNode() {
      var p = document.createElement('p');
      p.id = 'product-search-status';
      p.className = 'sr-only';
      p.textContent = statusMessage();
      return p;
    }

    function rowButton(product) {
      var btn = document.createElement('button');
      // type="button", never the default submit: the row must not submit the
      // add-line form implicitly; the island triggers it deliberately.
      btn.type = 'button';
      btn.className =
        'btn-secondary flex w-full select-none items-center justify-between gap-3 bg-surface text-left text-sm';
      btn.setAttribute('data-product-id', String(product.id));
      var name = document.createElement('span');
      name.className = 'min-w-0 truncate font-semibold';
      name.textContent = product.name;
      var meta = document.createElement('span');
      meta.className = 'shrink-0 text-xs text-muted';
      meta.textContent =
        product.sku +
        ' • ' +
        (priceKind === 'cost' ? messages.cost + ' ' + product.cost_price : product.sale_price) +
        ' • ' +
        messages.stock +
        ' ' +
        product.stock;
      btn.appendChild(name);
      btn.appendChild(meta);
      return btn;
    }

    // The visual list, derived from matches and status. While a search is in
    // flight the previous matches stay on screen (the way the old htmx swap
    // behaved), so a fast typist can still arrow into them; an empty state is
    // only rendered when a search has actually answered.
    function visualList() {
      if (state.status === 'idle') return null;
      if (state.matches.length === 0) {
        if (state.status !== 'done') return null;
        var empty = document.createElement('div');
        empty.className = 'empty px-3.5 py-2.5 text-start';
        empty.textContent = state.query === '' ? messages.idle : noResultsMessage();
        return empty;
      }
      var off = document.createElement('div');
      // The visual list is not announced; the polite region above it is.
      off.setAttribute('aria-live', 'off');
      var list = document.createElement('div');
      list.className = 'flex flex-col gap-1.5';
      state.matches.forEach(function (product) {
        list.appendChild(rowButton(product));
      });
      off.appendChild(list);
      return off;
    }

    function render() {
      var visual = visualList();
      if (visual) {
        results.replaceChildren(statusNode(), visual);
      } else {
        results.replaceChildren(statusNode());
      }

      // The busy cue is aria-hidden (the status announces the same state) and
      // shows through the stylesheet's htmx-indicator pair: the island adds
      // `htmx-request` while in flight, exactly what htmx used to toggle.
      busy.classList.toggle('htmx-request', state.status === 'searching');

      // The selection is island state, not markup state: the hidden input is
      // disabled while nothing is selected, so the typed-name path never
      // competes with a stale id on the wire.
      var selected = state.selectedProductId === null ? '' : String(state.selectedProductId);
      hidden.disabled = selected === '';
      hidden.value = selected;

      // Focus is derived, not restored: the same product keeps focus while it
      // is still a match; when it is gone focus returns to the field, where
      // the operator is typing. When no product holds focus, render() never
      // moves it (an operator typing in the qty field is not interrupted).
      if (state.focusedProductId !== null) {
        var again = results.querySelector(
          'button[data-product-id="' + state.focusedProductId + '"]'
        );
        if (again) {
          if (document.activeElement !== again) again.focus();
        } else {
          state.focusedProductId = null;
          input.focus();
        }
      }
    }

    // Track which product holds focus, so a re-search can follow it. Focusing
    // the field clears the tracked product, so a later re-search never steals
    // focus back to a button.
    container.addEventListener('focusin', function (evt) {
      var t = evt.target;
      if (t === input) {
        state.focusedProductId = null;
      } else if (t.matches && t.matches('button[data-product-id]')) {
        state.focusedProductId = t.getAttribute('data-product-id');
      }
    });

    // Exactly one activation path: the match's id is island state, the hidden
    // input carries it, and the ONE add-line form posts through htmx as
    // before (swap, OOB fragments and HX-Trigger events included).
    results.addEventListener('click', function (evt) {
      var btn =
        evt.target && evt.target.closest
          ? evt.target.closest('button[data-product-id]')
          : null;
      if (!btn) return;
      state.focusedProductId = btn.getAttribute('data-product-id');
      state.selectedProductId = Number(btn.getAttribute('data-product-id'));
      render();
      form.requestSubmit();
    });

    var timer = null;
    var inflight = null;

    input.addEventListener('input', function () {
      // A typed change invalidates a clicked selection: the two resolution
      // paths must not compete on the next submit.
      state.selectedProductId = null;
      state.query = input.value;
      if (timer) clearTimeout(timer);
      timer = setTimeout(runSearch, DEBOUNCE_MS);
    });

    function runSearch() {
      timer = null;
      // A newer keystroke supersedes an older request; the abandoned response
      // must never render. Aborting also keeps results in order.
      if (inflight) inflight.abort();
      var controller = new AbortController();
      inflight = controller;
      state.status = 'searching';
      render();

      fetch(SEARCH_URL + '?q=' + encodeURIComponent(state.query), {
        signal: controller.signal,
        headers: { Accept: 'application/json' }
      })
        .then(function (res) {
          if (inflight === controller) inflight = null;
          if (!res.ok) {
            return res.text().then(function (body) {
              searchFailed(body);
            });
          }
          return res.json().then(function (data) {
            state.matches = data.products || [];
            state.status = 'done';
            render();
          });
        })
        .catch(function (err) {
          if (inflight === controller) inflight = null;
          if (err && err.name === 'AbortError') return;
          searchFailed('');
        });
    }

    // A failed search has no results to swap in, so the island says so from
    // state. The live region is handled by render(); the visible notice
    // region is base.html's authority, so the failure re-dispatches the same
    // event shape base.html already consumes for the HTML picker on the
    // purchase page — one notice contract, two transports.
    function searchFailed(body) {
      state.status = 'failed';
      render();
      document.body.dispatchEvent(
        new CustomEvent('htmx:responseError', {
          detail: {
            requestConfig: { elt: input },
            pathInfo: { requestPath: SEARCH_URL },
            xhr: { responseText: body || '' }
          }
        })
      );
    }

    render();
  }

  function mountAll(root) {
    var pickers = (root || document).querySelectorAll('[data-picker]');
    for (var i = 0; i < pickers.length; i++) mount(pickers[i]);
  }

  document.addEventListener('DOMContentLoaded', function () {
    mountAll();

    // The add-line response brings a fresh picker island back. On the sale
    // page the picker swaps OUT OF BAND, so `htmx:load`'s `detail.elt` is the
    // new `#line-picker` itself; on the purchase page the picker travels
    // inside the swapped `#purchase-record-money` region instead, so `elt` is
    // the region and the island is found in its subtree. htmx:load is the
    // signal: the vendored htmx 1.9.12 fires it from the settle task of every
    // element it swaps in (the insert helper pushes the `htmx:load` trigger
    // for each inserted node), including out-of-band content — `oobSwap`
    // swaps through the same swap path and the event's `detail.elt` is the
    // new element itself. Mount what arrives; the dataset marker keeps mounts
    // idempotent.
    document.body.addEventListener('htmx:load', function (evt) {
      var elt = evt.detail && evt.detail.elt;
      if (!elt || !elt.matches) return;
      if (elt.matches('[data-picker]')) {
        mount(elt);
      } else if (elt.querySelectorAll) {
        mountAll(elt);
      }
    });
  });
})();
