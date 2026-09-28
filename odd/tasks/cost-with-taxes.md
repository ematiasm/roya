# Cost with taxes — bidirectionally, without a flag

## Status

**Design closed. Four decisions taken; four tasks built.**

**Progress: T-A merged as #120. T-B, T-C and T-D are implemented, mutation-proved
and awaiting their PRs.** The review receipt for T-B and T-C is outstanding: the
OpenCode review transport stopped producing capturable results after T-A's review
succeeded, so no lineage has a receipt yet.

The feature went through two rewrites. The first put a `prices_include_taxes`
flag on the purchase with a pre-fill on the supplier. The second dropped the flag
entirely. This is the third revision, and it is the one the user's answers
produced: **no flag, and the operator may enter either side — but never at the
line level.**

## The business fact, verbatim

> "al cargar una compra hay proveedores que discriminan impuestos y otros no,
> entonces poder cargar precio costo con o sin impuesto facilita mucho la carga de
> compras"

The pain is in **entry**. This design solves entry, and it does so without ever
asking anyone to declare a basis.

## What ARCA asks for

Checked against ARCA (ex-AFIP) sources, September 2026. **Secondary sources plus
official format specifications — not the normative text. A compliance answer needs
an accountant.**

| question | answer | this repo |
| --- | --- | --- |
| must prices be entered net? | **yes** — with gross prices the system must run the inverse, which "puede generar valores que no sean exactos", affecting the fiscal information sent | agrees (`line_taxes.rs`: "both are net") |
| do the rate sets cascade? | **no** — internal taxes on the net, IVA on the net *excluding* internal taxes | agrees — "deliberately additive and never compounding" |
| is the midpoint rounding method fixed? | **no** — the taxpayer determines and documents it, subject to fiscal neutrality | agrees — `MidpointAwayFromZero`, "the single half-up money rule" |
| may the document total differ from the sum of its components? | **no** — the Libro de IVA Digital rejects it | agrees — `total == net + Σ round2(net·rate_i/100)`, no adjustment line |

Per-record rounding is **forced by the format**, not chosen: every amount is 13
integers and 2 decimals and the total must equal the sum of its components.

> **RG 715/1999 is abrogated** by RG 5705/2025, effective 1 December 2025. The
> governing norm is the Libro de IVA Digital (RG 4597, RG 5707/2025). Any argument
> in this repo resting on RG 715 rests on a dead norm.

## Decisions, settled

1. **The net is the stored truth**, for the product's cost and sale price. No flag
   and no second stored number for the same figure.
2. **The tax set belongs to the product.** `product_taxes` maps
   `product_id → tax_id`; the only reader is `list_active_for_product`. Nothing
   else may influence a line's arithmetic.
3. **Rounding happens once, at the line**, half-up. Not per unit: rounding the unit
   and then multiplying scales the error by the quantity, and always in the same
   direction. Worked example, cost 0,03 at 21% and quantity 7 — per unit gives
   0,28 where line-level gives 0,25, and the true figure is 0,2541.

   **The error bound is half a cent PER LINKED TAX, and it does not involve the
   quantity.** For `n` taxes and a net already at money scale:
   `|total − net·(1 + Σrateᵢ/100)| ≤ n · cent()/2`. Measured maxima: `0,005` at one
   rate, `0,0075` at two, `0,0125` at three. The quantity-independence is the
   property that matters and it is exactly true; the bare "half a cent" is true
   only for a single-rate line, and this entry over-claimed it until T-D measured
   it. A net carrying **more** than two decimals pays one further half cent for
   the line's own pin, so its bound is `(n+1) · cent()/2`.

   Pinned as a property over 450 combinations in `line_taxes.rs`, against an
   unrounded yardstick. A per-unit bug also **cannot live** inside
   `calculate_line_taxes`: it receives the line net and no `qty`, so the per-unit
   design is structurally impossible at that seam.
4. **The unit cost is what is stored; the line total is always derived.**
   `purchase_lines` keeps `qty` + `unit_cost`, frozen at confirm. Nothing divides
   a line total back into a unit cost, ever.

### Why decision 4 is not merely a preference

A line total is `qty × (cost + taxes)`. Undoing it to recover the unit cost is a
**division**, and it does not come back:

| | |
| --- | --- |
| line gross entered | 121,00 |
| qty 3 at 21% | line net solves exactly to 100,00 |
| `100,00 ÷ 3` | 33,3333… |
| rounded | **33,33** |
| re-priced as a line | 3 × 33,33 = 99,99 net → **120,99** gross |

The cent is gone, and every later update rounds the same way. Re-entering the line
total is therefore **not offered at all**.

The unit level has no such problem: `solve_net_from_gross` does not divide. It
searches for a net whose gross is **exactly** the typed figure, so for every
reachable figure the inverse is exact, and for the staircase gaps it **refuses**
rather than approximating. That refusal is `CostUnreachable`, and it already
exists.

## The four figures

```
cost_price ─────────────────────────►  derived: cost with taxes
             │
             └─ markup ──► sale_price ──►  derived: sale price with taxes
```

| | figure | where it lives | with taxes |
| --- | --- | --- | --- |
| 1 | cost, net | `products.cost_price` | no |
| 2 | **cost with taxes** | **derived — missing today** | yes |
| 3 | sale price, net | `products.sale_price` | no |
| 4 | sale price with taxes | derived on the ladder, already built | yes |

Figure 2 is the only one that does not exist. Its arithmetic is
`calculate_line_taxes(cost_price, &taxes).total`, and that contract already exists.

## Where a purchase's cost lands

**A purchase never writes `products.cost_price`.** That is acceptance criterion
AC10, and `SupplierService::record_cost` says so in its own doc comment:
*"`products.cost_price` is deliberately untouched."* A confirmed purchase writes
the **satellite**, `product_supplier_costs.current_cost`, per product+supplier
pair, with `previous_cost` and its own date. The product's `cost_price` is a
separate, later decision derived from the chosen supplier.

This separation is what contains rounding drift. A cent of error in one
supplier's satellite does not silently restate the product's master cost, and
therefore does not silently restate its `markup_pct` and its `sale_price`. It has
to travel through the product's own cost decision to get there.

## Entry

Two form fields, side by side, on the purchase line:

- **costo** (net) — authoritative
- **costo con impuestos** — derived, and **also editable**

Whichever the operator types into becomes the input and the other is solved. This
is the ladder's existing pattern, not a new mechanism. It needs one piece of form
state — which field was typed last — so the two never drive each other in a loop,
and a refusal on the gross side has somewhere to render.

## Tasks

- [x] **T-A — the cost's tax-inclusive figure.** `ProductPriceLadder.cost_total`,
  a `Decimal` beside `net_total` and `total`, filled from
  `calculate_line_taxes(cost_price, &taxes).total`. **Zero new arithmetic** — the
  contract exists and the ladder already calls it for the sale. The computation is
  unconditional and independent of `net_refusal`, because the cost gross is a
  *different fact* from the net's refusal: a manual-price product may legitimately
  carry `cost > net`, and the cost gross can overflow where the net gross does
  not. A refused cost carries no amount — `Decimal::ZERO` plus a distinct
  `cost_refusal: Option<PriceRefusal>` — so the template guards on the refusal and
  never prints the zero. Landed in `bf637db`, `dc7cf6d`, `05fd08c`.
- [x] **T-B — the ladder row.** In `templates/partials/product_price_ladder.html`
  as a **4-`<td>` `<tr>`**, placed between the cost and the markup. `e2e/tests/
  test_products.py::_ladder_amounts` keys every figure by `td[0]` and reads
  `td[3]`, and it **skips rows with fewer than 4 cells** — so a 3-cell row is
  invisible to all six browser tests. Landed in `2a83fc2`.

  > **Correction, measured during T-B:** that warning is **one-directional**. A row
  > with **5** cells is *not* skipped — `td[3]` reads the wrong cell and the browser
  > suite stays green. The only guard against a wider row is the cell-count
  > assertion in the Rust route test. Anything relying on the 4-cell shape has to
  > assert the count, not assume the helper enforces it.
- [x] **T-C — bidirectional entry on the purchase line.** Two fields on the
  purchase line, net and gross, on both write paths and the entry row's live
  preview. `src/services/purchase_cost.rs` owns the boundary: it delegates the cent
  search whole to `solve_net_from_gross`, re-words the three **sale** refusals it
  actually returns into the four `Cost*` variants through one mapping, and holds
  the direction rule as a pure function so create, edit and preview cannot drift.
  Landed in `9ba77f1`…`a935e61`.

  > **Two facts the plan did not carry.** (1) T-C was **never dependent on T-A** —
  > `purchases_web.rs` had zero references to the ladder; it needs only #117 and
  > #119, both long merged, so it was cut from `main` and never deepened the
  > chain. (2) The inline row needs **no** client-side mirror: the PUT's answer
  > *is* the record re-rendered from storage, so the server is that row's mirror
  > and a refused gross edit falls out of the existing `defaultValue` rollback with
  > no new state. Only the entry row carries `data-last-edited`, because only there
  > can an answer arrive late.
- [x] **T-D — tests.** The entry directions, the gross-of-no-net refusal, the merge
  proof and the exactness pin all landed with T-C. What T-D added is the
  **line-level-vs-per-unit counter-example** in `line_taxes.rs`, stated as a
  property over 450 combinations rather than as fixtures — which is what corrected
  Decision 3's bound above. Landed in `67633a1`.

## Already delivered, and why it is not dead

Both shipped fully reviewed, and under a design that briefly looked like it had no
caller. It does.

| | shipped in | builds | caller |
| --- | --- | --- | --- |
| `solve_net_from_gross` | [#117](https://github.com/ematiasm/roya/pull/117) | gross → net, the cent search | **T-C** |
| `CostUnreachable`, `CostNotInvertible`, `CostNetTooLarge`, `TaxRateTooLargeToCost` | [#119](https://github.com/ematiasm/roya/pull/119) | refusals for that search | **T-C** |

`solve_net_from_gross` takes a **net amount**, not a unit price, so it is already
the right granularity for a unit cost. Its divisor parameter is what keeps
`solve_final_price`'s six ordered rules intact, and its refusals are the only
reason the exactness of the inverse is provable rather than asserted.

## Constraints

- **No new money rounding rule.** `round_to_cents` stays the only rounding and
  `calculate_line_taxes` stays the only definition of a gross.
- **The net is the truth.** No second stored number for the same figure.
- **Nothing divides a line total into a unit cost.** Not a form, not a service,
  not a repair path.
- **The document total is the exact sum of its components.** No adjustment line.
- **A refused figure publishes no amount** — not a zero, not a partial, not a
  stale one.
- **Strict TDD**, mutation-proved. A test that survives the mutation it was
  written for is decorative.
- **Technical artifacts in English.**
- Never stage `odd/tasks/pos-counter-sales.md` or
  `odd/tasks/residual-interface-scope.md`. Never open `roya.db`; test pools are
  `sqlite::memory:`.

## Two facts that will mislead a reader

1. **`taxes` is EMPTY in production.** Migrations seed nothing and
   `src/t1_schema_tests.rs:164-175` asserts it. Every `IVA 21` / `IVA 10,5` in the
   tree is a **test fixture**. There is no default tax catalogue.
2. **The tax model has no heterogeneity to complicate the inverse.** One `rate`
   column, percentage of net, `0…1000`, no `basis` column, no fixed-amount tax.
   "EXENTO" is a row with `rate = '0'` and the arithmetic still runs. So every
   rate set here is invertible as a whole or not at all, and the only failure
   modes are the staircase gaps and the ceiling.

## Out of scope, and it is not small

**The Libro de IVA Digital.** This app does not generate the file and nothing
here requires it to. But the format is unambiguous where it touches this code:
13+2 decimals with more rejected, the total equal to the sum of its components,
one base per alícuota so the non-cascading model is structurally enforced, a
"cantidad de alícuotas" field, internal taxes as a separate amount, and 4/6
exchange rates. `Decimal` carries all of it. Generating the file is a separate
feature with its own plan.

**Per-document rounding with an adjustment concept.** The TEAC held that rounding
per operation "distorsiona de manera significativa el importe a declarar en la
autoliquidación", and at least one provider works that way: full precision
internally, round the document, carry the cent in a non-taxable "Ajuste de
redondeo". That is a legitimate second architecture and it is deferred, not
rejected. It needs a contador to decide what the autoliquidación is expected to
look like.
