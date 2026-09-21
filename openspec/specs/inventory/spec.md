# Capability: inventory (M1)

## Purpose
Know what is in the shop, what is running out, and what to buy — for both goods and services.

## Entities

### categories
`id`, `name`, `parent_id` → categories (nullable). Free tree, any depth, root when `parent_id IS NULL`.
`UNIQUE(parent_id, name)` so a name may repeat in different branches.

### products
`id`, `sku` (UNIQUE, ≤ 64), `name` (≤ 128), `kind` (`Product` | `Service`), `category_id` (nullable,
ON DELETE SET NULL), `unit` (short free text: `un`, `kg`, `lt`, `m`, `hs`), `sale_price`, `cost_price`,
`markup_pct` (nullable — the percentage applied over the cost to derive the sale price; NULL means
"no markup, manual price"), `track_stock`, `min_stock`, `max_stock`, `location`, `notes`,
`is_active`, `created_at`, `updated_at`.

### product_barcodes
`id`, `product_id` → products ON DELETE CASCADE, `code` (UNIQUE). Zero, one or many per product.

### stock_movements
`id`, `product_id` → products ON DELETE RESTRICT, `qty`, `type` (`In` | `Out` | `Adjust`),
`reason` (`Purchase` | `Sale` | `Loss` | `Adjust` | `Initial` | `Sale-return` | `Purchase-return`),
`reference` (free text carrying the source document number), `date`, `created_at`.

## Rules
- SKU is unique; a duplicate returns 409. Category names are unique within the same parent.
- **A set markup derives the sale price from the cost:** `sale_price = cost_price *
  (1 + markup_pct/100)`, computed server-side in `validate_product`, the single validation entry
  for both create and update. The cost underneath is `products.cost_price`, which purchases
  deliberately do NOT maintain — a purchase updates the per-supplier `product_supplier_costs`
  satellite instead, and `cost_price` is only the fallback for products with no supplier row — so
  a markup tracks the cost the operator maintains on the product. When a markup is set the
  submitted `sale_price` is ignored; when
  `markup_pct` is NULL the price is manual and stored verbatim — NULL is a value, not an absence,
  and is deliberately not `0`, which would pin the price to the cost.
- **The stored price never contradicts its parts.** The recomputation happens at write time, so at
  rest a product with a markup always holds exactly the derived price. This is a service-level
  invariant, not a database CHECK (SQLite cannot do decimal arithmetic on TEXT columns), and no
  historical document depends on it: `sale_lines.unit_price` snapshots the price when a line is
  built, never referencing the product.
- `markup_pct <= -100` is rejected; a set markup with `cost_price <= 0` is rejected, so a product
  can never be silently derived to a free price.
- The price rule (`sale_price > 0` for `Product`, `>= 0` for `Service`) applies to the effective
  price, so a caller that supplies a markup is not also required to supply a meaningful price
  value; a derived `0.00` is refused for a `Product` and accepted for a `Service`, which may be
  free. On `POST /api/products` the `sale_price` key itself stays mandatory — omitting the key
  fails deserialization before the service runs — and only its value is ignored when a markup is
  set.
- Clearing the markup keeps the last stored price and returns the product to a manual price; on
  the API it is clearable via explicit `null`, an absent key leaving it unchanged.
- The derived price is pinned to cents with half-up rounding (`10.005 → 10.01`) — the project's
  only rounding; manual prices stay exact. The percentage is applied as a multiplication by
  `Decimal::new(1, 2)`: the project never divides a `Decimal`.
- A malformed stored `markup_pct` degrades to "no markup, manual price", never to 0%, which would
  silently pin the price to the cost.
- **Prices render through `money_display`.** The display normalises the scale up to exactly two
  decimals when the stored value already fits in two, and returns anything finer untouched — it
  never rounds, because showing `7.78` for a stored `7.777` would misstate the price the customer
  is charged.
- **Service products cannot hold stock:** a `Service`, or any product with `track_stock = false`,
  rejects stock movements and cannot carry `min_stock` or `max_stock`.
- `track_stock = true` requires `min_stock >= 0` and `max_stock >= min_stock`.
- **No category cycles:** a category cannot be its own parent nor a descendant of itself. Deleting a
  category with children or with products is refused; rename or move instead.
- Stock movements with `qty <= 0` are rejected (except `Adjust`, which accepts a signed non-zero delta),
  and a movement for an unknown or inactive product is rejected.
- **Derived stock:** `SUM(In) − SUM(Out) ± Adjust`. Never stored as truth.
- **Negative stock is permitted by default** (`ALLOW_NEGATIVE_STOCK=true`), because real counting drifts
  behind the system and blocking a sale at the counter creates daily friction. Negative stock is not an
  error, it is a flagged state, corrected later by an adjustment. With the flag set to false, an `Out`
  that would go below zero is rejected with 400.
- **Derived reads:** `low_stock` when `stock <= min_stock`; `negative_stock` when `stock < 0`;
  `suggested = max_stock − stock` when `stock <= min_stock`, which grows automatically if stock is
  negative.
- Deleting a product with stock history is refused (RESTRICT); deactivate with `is_active = 0` instead.

## Interface
- REST: `GET/POST /api/categories`, `PUT/DELETE /api/categories/{id}`, `GET/POST /api/products`,
  `GET/DELETE /api/products/{id}`, `GET /api/products/{id}/stock`, `GET/POST /api/products/{id}/barcodes`,
  `GET/POST /api/stock-movements`, `GET /api/low-stock`, `GET /api/negative-stock`.
- Web: `/products` with the product list, low and negative stock badges, the reorder suggestion and the
  create forms. Both the create modal and the product drawer have a "Markup %" input, but the two
  price fields differ: the product drawer has a stored product, so when a markup is present its
  price input renders `readonly` server-side showing the price the server last derived, with a
  hint that it is recalculated on save; the create modal has no stored product, so it shows no
  derived value and no hint, and its price field's `readonly` state is set by client-side script
  that watches the markup field. In both the `readonly` attribute is courtesy only, the handler is
  the enforcement.
- Configuration: `ALLOW_NEGATIVE_STOCK` (default `true`).

## Authorization
Every inventory route requires the permission its action declares, enforced by the security kernel —
the route → permission table in `identity/spec.md` is the authority. The product drawer
(`/web/products/detail/{id}`) is double-gated `inventory.read` AND `purchases.costs.read`: it
renders per-supplier cost rows owned by the purchases department, and its cost forms write with the
data owner's code (`purchases.costs.write`) even though the form lives in this department's page.
This is also a cross-capability pair: the product picker other screens read (`/web/product-search`)
is `inventory.read`, so a sales recorder needs the inventory read it holds in every seeded matrix.

## Verification
`src/services/inventory.rs` (acceptance criteria AC1–AC7 and their triangulation cases) and
`src/routes/inventory_api.rs`. The smoke suite exercises the product and stock paths through the real
router.
