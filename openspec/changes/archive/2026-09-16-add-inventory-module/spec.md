# Spec: add-inventory-module

## Entities

### categories
- `id INTEGER PK`, `name TEXT NOT NULL`, `parent_id INTEGER NULL -> categories(id)`
- Constraints: `UNIQUE(parent_id, name)`, `CHECK(id != parent_id)`
- Semantics: free tree, any depth. Root = `parent_id IS NULL`.

### products
- `id PK`, `sku TEXT NOT NULL UNIQUE`, `name TEXT NOT NULL`
- `kind TEXT CHECK(kind IN ('Product','Service'))`
- `category_id NULL -> categories(id) ON DELETE SET NULL`
- `unit TEXT NOT NULL` (simple, e.g. `un, kg, lt, m, hs`)
- `sale_price TEXT NOT NULL` (Decimal as TEXT, same pattern as finance in SQLite)
- `cost_price TEXT NOT NULL DEFAULT '0'`
- `track_stock INTEGER NOT NULL` (0/1)
- `min_stock TEXT NULL`, `max_stock TEXT NULL` (Decimal-as-TEXT, quantities)
- `location TEXT NULL`, `notes TEXT NULL`
- `is_active INTEGER NOT NULL DEFAULT 1`
- `created_at TEXT`, `updated_at TEXT`
- Rules:
  - `name` trimmed, non-empty, <= 128.
  - `sku` trimmed, non-empty, <= 64, UNIQUE.
  - `sale_price > 0` when `kind=Product`; `>= 0` allowed for internal use.
  - `cost_price >= 0`.
  - `kind=Service` => `track_stock=0`, `min_stock/max_stock=NULL`, `location=NULL` allowed.
  - `track_stock=1` => `min_stock >= 0`, `max_stock >= min_stock` (both required).
  - `unit` trimmed, 1..16 chars.

### product_barcodes
- `id PK`, `product_id NOT NULL -> products(id) ON DELETE CASCADE`
- `code TEXT NOT NULL UNIQUE`, trimmed 1..64
- `created_at TEXT`

### stock_movements
- `id PK`, `product_id NOT NULL -> products(id) ON DELETE RESTRICT`
- `qty TEXT NOT NULL` (input always > 0, sign derived from `type`)
- `type TEXT CHECK(type IN ('In','Out','Adjust'))`
  - `In` = +qty, `Out` = -qty, `Adjust` = signed qty (positive or negative delta)
- `reason TEXT CHECK(reason IN ('Purchase','Sale','Loss','Adjust','Initial'))`
- `reference TEXT NOT NULL DEFAULT ''` (free text, future sale/purchase id)
- `date TEXT NOT NULL (NaiveDate)`, `created_at TEXT`
- Rules: product must exist and be active; `kind=Service` or `track_stock=0` rejects.

## Derived queries (never stored as source of truth)
- `stock(product) = SUM signed qty`
- `low_stock = track_stock=1 AND stock <= min_stock`
- `negative_stock = track_stock=1 AND stock < 0`
- `reorder_suggestion = max_stock - stock` when `stock <= min_stock` (grows if negative)

## Config
- `ALLOW_NEGATIVE_STOCK: bool, default true` (env, same pattern as `ALLOW_NEGATIVE_BALANCE`)
  - `true`: `Out` allowed even if projected < 0.
  - `false`: `Out` with projected < 0 rejected with 400.

## API / Web (mirror existing patterns)
- REST under `/api/products`, `/api/categories`, `/api/stock-movements`,
  `/api/products/:id/stock`; Web under `/products`, HTMX fragments like M0.
- Errors reuse `AppError`: 400 validation, 404 not found, 409 duplicate sku/barcode.

## Acceptance criteria
- [ ] AC1: create Product with sku unique ok; duplicate sku => 409.
- [ ] AC2: create Service with `track_stock=0`, min/max NULL ok; movement for Service => 400.
- [ ] AC3: movement with `qty <= 0` => 400; unknown product => 404.
- [ ] AC4: category cycle (self-parent or descendant-parent) => 400.
- [ ] AC5: delete category with children or products => 400; delete empty leaf ok.
- [ ] AC6: `ALLOW_NEGATIVE_STOCK=false` + Out exceeding stock => 400, stock unchanged.
- [ ] AC7: `ALLOW_NEGATIVE_STOCK=true` + Out exceeding stock => 201, stock negative, appears in negative list.
- [ ] AC8: stock == sum of signed movements (checked via `GET stock` after In/Out/Adjust).
- [ ] AC9: `stock <= min` => appears in low-stock list with `suggested = max - stock`.
- [ ] AC10: duplicate barcode across products => 409; delete product cascades its barcodes, never touches `transactions`.
- [ ] AC11: finance tables untouched by any inventory op (no new `transactions` rows).
