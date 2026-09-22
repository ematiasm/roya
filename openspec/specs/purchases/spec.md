# Capability: purchases (M3)

## Purpose
Buy merchandise, know what each supplier charges and when that price changed, and let the reorder
suggestion become an actual purchase order — the mirror of sales on the money-out side.

## Entities

### suppliers
`id`, `name` (UNIQUE, ≤ 128), `phone`, `notes`, `is_active`, `created_at`, `updated_at`.

### product_supplier_costs
`id`, `product_id` → products ON DELETE RESTRICT, `supplier_id` → suppliers ON DELETE RESTRICT,
`current_cost`, `current_cost_updated_at`, `previous_cost`, `previous_cost_updated_at`,
`is_preferred`, `supplier_sku`, `created_at`, `UNIQUE(product_id, supplier_id)`.

This satellite is the only place a product meets a supplier. `products` holds no supplier reference.
A partial unique index enforces at most one preferred supplier per product.

### purchases
`id`, `purchase_number` (UNIQUE, nullable), `supplier_id` → suppliers ON DELETE RESTRICT,
`status` (`Draft` | `Confirmed` | `Cancelled`), `payment_type` (`Cash` | `Credit`),
`purchase_date`, `due_date`, `supplier_invoice_no`, `notes`, `cancel_reason`, and the same audit
timestamps as a sale.

### purchase_lines
`id`, `purchase_id` → purchases ON DELETE CASCADE, `product_id` → products ON DELETE RESTRICT,
`qty` (> 0), `unit_cost` (frozen at confirmation).

### purchase_payments
Same shape as `sale_payments`: `purchase_id`, `account_id`, `method_id`, `amount`, `date`,
`transaction_id`, `refund_transaction_id`.

## Rules
- **A draft is the purchase order.** It can be built from the reorder suggestion and edited freely;
  it touches no stock, no finance and no cost until confirmed.
- **Confirmation** assigns `YYYY-PURCH-NNNNNN`, freezes line costs, issues one `In` movement with reason
  `Purchase` per stock-tracked line, and then records one payment and one `Expense` for cash, or leaves
  a payable for credit.
- **Reception is complete.** There is no partial receiving; goods arrive together.
- **Cost rule.** On confirmation the satellite is updated per line: a cost that *differs* from the
  current one shifts the current value and its date into `previous`, and the new value becomes current.
  A repeated identical cost refreshes only the date, so `previous` keeps the last genuinely different
  price and the derived raised/lowered alert stays meaningful (`100 → 120 → 120 → 120` still reports
  a raise against 100). The alert is derived; nothing is stored.
- **A purchase never writes `products.cost_price`.** That column is the fallback for products with no
  supplier row, such as services. The read rule is: if the satellite has rows for the product, the
  satellite wins; otherwise the column.
- **Reference cost** is the preferred supplier's current cost, else the cheapest, else none.
- **Reorder suggestion** lists products at or below `min_stock` with `suggested = max_stock − stock`,
  the chosen supplier and its satellite cost. Products with no satellite row are returned in a separate
  `without_supplier` list and are never silently dropped.
- A product may appear only once per purchase, for the same reason as sales.
- **Cancelling** a confirmed purchase returns the goods to the supplier (`Out`, reason
  `Purchase-return`) and refunds the money paid as an `Income` per payment. Unlike the sales refund,
  this is money *entering* the account, so it cannot overdraft and needs no balance guard. The
  partial-annulment refusal still applies: if any payment already carries a `refund_transaction_id`,
  the annulment was partially applied by an earlier attempt and is refused, not doubled.
- **Supplier payment.** One handover of money is allocated oldest debt first (`due_date`, then
  `purchase_date`, then id) across the supplier's Confirmed credit purchases, one payment per covered
  purchase, each still posting its own `Expense` with `reference = purchase_number`. Suppliers have no
  grouping receipt document (unlike customer receipts), so there is no receipt id and no total to
  derive: the result is the created payments. More than the outstanding debt ⇒ 400 naming both
  figures; an unassigned/inactive method ⇒ 400; both are raised before any write.
- Deleting a supplier or product with history is refused (RESTRICT); deactivate instead.

## Interface
- REST: `GET/POST /api/suppliers`, `GET/PUT /api/suppliers/{id}`, cost endpoints,
  `GET/POST /api/purchases`, `GET/PUT /api/purchases/{id}`, lines, payments, `confirm`,
  `cancel`, `GET /api/purchases/suggestions`, and `POST /api/supplier-payments` (supplier-level
  payment, oldest-first, no receipt document).
- Web: `/purchases` with the purchase list, the "Sugerido" panel that seeds a draft, the draft line
  editor and the confirm, pay and cancel forms; a draft line whose cost is strictly higher than
  the product's stored cost, when that stored cost is non-zero (zero means no cost recorded yet),
  renders a stale-cost warning with an "Apply to product" action
  (`POST /web/purchases/{purchase_id}/lines/{line_id}/apply-cost`, draft-only, gated
  `inventory.write`); `/suppliers` with the cost satellite and its
  raise/lower badge, and the drawer detail with the pay-supplier form (`POST /web/supplier-payments`)
  and the record-cost form.

## Authorization
Every purchases and suppliers route requires the permission its action declares, enforced by the
security kernel — the route → permission table in `identity/spec.md` is the authority. Two
consequences are recorded deliberately: a payment to a supplier is `purchases.create`, not
`suppliers.write` (the payment is a purchase-side money movement; entity editing must not hand over
money movements — a `suppliers.write`-only principal sees the pay card in the drawer it may open and
is refused on submit), and the reorder suggestions are `inventory.read` (stock-derived data, not a
purchase document), so the `/purchases` page renders its Sugerido block conditionally on that read.
The purchase record page itself is a single `purchases.read` gate: the line costs and payments it
renders are purchases data. The third deliberate consequence is the apply-cost action: a route
that lives on the purchases page (`POST /web/purchases/{purchase_id}/lines/{line_id}/apply-cost`)
is gated `inventory.write`, not `purchases.create`, because its effect lands on the product — it
writes the line's cost into `products.cost_price` through the inventory service, and gating it
with a purchase permission would hand a buyer the ability to rewrite product costs. This does not
weaken the rule that the purchase flow never writes `products.cost_price`: the action is a
separate, human-triggered product write that reaches the product through the inventory service,
and the confirmation path still touches only the satellite.

## Verification
`src/services/purchases.rs` and `src/services/suppliers.rs` (AC1–AC14 and the cost-rule cases),
`src/routes/purchases_api.rs`, `src/routes/purchases_web.rs`, `src/routes/suppliers_web.rs`, and the
purchase flow in `src/smoke_tests.rs`.
