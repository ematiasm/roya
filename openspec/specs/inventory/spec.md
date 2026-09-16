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
`track_stock`, `min_stock`, `max_stock`, `location`, `notes`, `is_active`, `created_at`, `updated_at`.

### product_barcodes
`id`, `product_id` → products ON DELETE CASCADE, `code` (UNIQUE). Zero, one or many per product.

### stock_movements
`id`, `product_id` → products ON DELETE RESTRICT, `qty`, `type` (`In` | `Out` | `Adjust`),
`reason` (`Purchase` | `Sale` | `Loss` | `Adjust` | `Initial` | `Sale-return` | `Purchase-return`),
`reference` (free text carrying the source document number), `date`, `created_at`.

## Rules
- SKU is unique; a duplicate returns 409. Category names are unique within the same parent.
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
  create forms.
- Configuration: `ALLOW_NEGATIVE_STOCK` (default `true`).

## Verification
`src/services/inventory.rs` (acceptance criteria AC1–AC7 and their triangulation cases) and
`src/routes/inventory_api.rs`. The smoke suite exercises the product and stock paths through the real
router.
