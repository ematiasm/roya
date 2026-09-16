# Roya — Local Personal Finance Tracker

Local CRUD for personal finances: 2–3 accounts/wallets, income/expense transactions, derived balances, REST API + HTMX web UI. 100% local, ready to scale.

Stack: **Rust + Axum 0.8.9 + Tokio + SQLx 0.9 (SQLite → Postgres) + Askama + HTMX + rust_decimal + chrono**.

## Features

- **Account** — `id, name, cached_balance, created_at`
- **Transaction** — `id, account_id (FK), kind (Income/Expense), amount (Decimal), description, reference (nullable, opaque), date (NaiveDate), created_at`
- Balance is **always derived** `SUM(Income) - SUM(Expense)` — `cached_balance` is kept in sync transactionally but never trusted for reads.
- Validation: `amount > 0`, negative balance blocked when `ALLOW_NEGATIVE_BALANCE=false`.
- Dashboard with total balance across all accounts.
- **REST API** and **Web (Askama + HTMX, no page reload)**.
- **Money traceability** — every payment row knows the finance transaction it
  produced (`sale_payments.transaction_id` / `purchase_payments.transaction_id`,
  FK to `transactions(id)` RESTRICT) and the refund it received
  (`refund_transaction_id`); the transaction carries the source document number
  in the immutable `transactions.reference` (`YYYY-SALE-NNNNNN` /
  `YYYY-PURCH-NNNNNN`), while `description` stays user-editable free text.
  Manual transactions have `reference = NULL`. Historical payments keep
  `transaction_id = NULL` (when several transactions share a document number
  there is no deterministic way to know which one a payment created), but they
  are still fully traceable through `reference`.
- **Sales** — Draft → Confirmed → Cancelled orchestrator (Odoo-style):
  - Draft creates lines with no stock/finance side effects.
  - Confirm assigns `YYYY-SALE-NNNNNN`, deducts stock (`Out`, reason `Sale`),
    Cash posts 1 Income with method, Credit opens a receivable (due = total).
  - Payments carry `account_id + method_id` (N per sale, mixed accounts/methods,
    sum ≤ total); each posts 1 Income; overpay ⇒ 400; Paid when due = 0.
  - `account_payment_methods` allowlist enforced (400) before any stock/sequence/
    finance touch.
  - Cancel of Confirmed re-enters stock (`In`, reason `Sale-return`) and posts
    Expense refunds, guarded by `ALLOW_NEGATIVE_BALANCE`.
  - `sale_number` UNIQUE, immutable, NULL only in Draft/Cancelled-from-Draft.
  - Finance/stock rows are written only via services, reference = `sale_number`;
    each payment stores the id of the Income it created.
- **Suppliers (M3)** — `suppliers(id, name UNIQUE, phone, notes, is_active)` plus the
  `product_supplier_costs` satellite holding the per-supplier price with its previous
  value and date. A new cost that differs shifts current → previous (value and date);
  a same-cost confirmation only refreshes the date so the last distinct price survives.
  The raised/lowered alert is derived (previous vs current), never stored.
  `products.cost_price` stays the fallback for products with no satellite row; delete
  is RESTRICT-aware (deactivate instead).
- **Purchases (M3)** — Draft → Confirmed → Cancelled orchestrator mirroring sales:
  - Draft (the pedido) is edited freely and touches no stock, no finance and no
    satellite cost; it can be seeded from the suggestion panel.
  - Confirm assigns `YYYY-PURCH-NNNNNN`, receives stock (`In`, reason `Purchase`) per
    tracked line, updates the satellite cost per line, Cash posts 1 payment + 1 Expense,
    Credit stays payable (`due = total`, no Expense until paid).
  - Credit payments each post 1 Expense (`reference = purchase_number`) and the
    payment stores the Expense id; N per purchase,
    mixed accounts/methods, sum ≤ total; Paid when due = 0; overpay ⇒ 400.
  - Cancel of Confirmed returns stock (`Out`, reason `Purchase-return`, the M1 CHECK
    expansion) and posts Income refunds per payment; a refund is money entering, so the
    balance guard never blocks it. Draft cancel is a discard with no side effects.
  - `account_payment_methods` allowlist enforced (400) before any stock/sequence/
    finance touch; `purchase_number` UNIQUE, immutable, NULL only while Draft (or
    cancelled before ever being confirmed).
- **Sugerido (purchase suggestion)** — low-stock tracked products with suggested
  qty = `max_stock − stock`, the chosen supplier (preferred satellite row, else cheapest
  current cost), satellite cost and subtotal. Products without a satellite row are
  returned in `without_supplier`, never silently dropped.
- **Payment methods (M0)** — `payment_methods(id, name UNIQUE, is_active)` seeded
  `Cash, Transfer, Debit, CreditCard, QR` (no `Other`); `account_payment_methods`
  allowlist `PK(account_id, method_id)` RESTRICT both; `sale_payments.method_id`
  RESTRICT NOT NULL. `sales` has no `account_id`.
  The allowlist is **explicit configuration, never a silent default**: the dashboard
  create-account form requires ticking at least one method, the account detail page
  (`/accounts/:id`) lets you replace the set, and REST uses
  `GET/PUT /api/accounts/:id/payment-methods` (empty list ⇒ 400, unknown id ⇒ 404).
  Accounts with no methods are flagged on the dashboard list and detail page, and a
  payment on them fails with a 400 that points to the configuration.
  Migration 12 still seeds `Caja→Cash`, `Banco→Transfer,Debit,CreditCard`,
  `MP→QR,Transfer` for accounts that already exist, and
  `PaymentMethodService::ensure_defaults_for_account` keeps that mapping reusable;
  no accounts are auto-created.

## Architecture

```
handler (routes/) -> service (services/) -> repository trait (repositories/) -> db
```

No SQL inside handlers. Repositories are `trait`-based so swapping SQLite for Postgres requires only a new trait impl, not rewriting handlers/services.

```
src/
  main.rs        — bootstrap, pool, router
  models.rs      — entities + DTOs
  db.rs          — pool + embedded migrations
  error.rs       — thiserror + IntoResponse
  services/      — business rules (balance guards, validation)
  repositories/  — AccountRepository / TransactionRepository traits + SQLite impls
  routes/        — api.rs (JSON) + web.rs (Askama/HTMX)
templates/       — Askama templates (base, dashboard, account_detail, partials)
migrations/      — sqlx-cli migrations
```

## Quick Start

```bash
# 1. Requirements: Rust 1.85+, sqlx-cli (optional, for new migrations)
cargo install sqlx-cli --no-default-features --features sqlite

# 2. Clone & run
git clone <repo>
cd roya

# 3. Env (optional)
cp env.example .env
# DATABASE_URL=sqlite://roya.db
# ALLOW_NEGATIVE_BALANCE=false
# ALLOW_NEGATIVE_STOCK=true
# RUST_LOG=info

# 4. Run (migrations run automatically via sqlx::migrate! at startup)
cargo run
# -> http://localhost:3000

# 5. Override port / db
DATABASE_URL=sqlite://roya.db PORT=3000 cargo run
ALLOW_NEGATIVE_BALANCE=true cargo run   # allow overdraft
```

Visit `http://localhost:3000` for the dashboard. REST examples below.

Static assets (`static/tailwind.css` and `static/htmx.min.js`) are committed and served locally at `/static/*`, so there is no CDN or npm setup. See [Styles & local assets](#styles--local-assets) to rebuild the CSS.

## Migrations (sqlx-cli)

Migrations live in `migrations/*.sql` and are **embedded** (`sqlx::migrate!("./migrations")`), so `cargo run` applies them even without `sqlx-cli`. Use `sqlx-cli` only to create new ones:

```bash
# Create new migration
sqlx migrate add create_foo --source migrations

# Apply pending (alternative to cargo run embedded)
DATABASE_URL=sqlite://roya.db sqlx migrate run

# Info
sqlx migrate info --source migrations
```

Current migrations:

- `20240101000001_create_accounts.sql` — `accounts`
- `20240101000002_create_transactions.sql` — `transactions` (FK, indexes, CHECK kind)
- `20240101000003_create_categories.sql` — `categories` (self-FK, UNIQUE parent+name)
- `20240101000004_create_products.sql` — `products` (UNIQUE sku, category FK SET NULL)
- `20240101000005_create_product_barcodes.sql` — `product_barcodes` (CASCADE, UNIQUE code)
- `20240101000006_create_stock_movements.sql` — `stock_movements` (RESTRICT, CHECK type/reason)
- `20240101000007_create_doc_sequences.sql` — `doc_sequences` PK(doc_type, year)
- `20240101000008_create_sales.sql` — `sales` (UNIQUE sale_number NULL-distinct, CHECKs)
- `20240101000009_create_sale_lines.sql` — `sale_lines` (CASCADE sale, RESTRICT product)
- `20240101000010_create_sale_payments.sql` — `sale_payments` (CASCADE sale, RESTRICT account)
- `20240101000011_expand_stock_reason_sale_return.sql` — adds `Sale-return` reason
- `20240101000012_payment_methods.sql` — `payment_methods` + `account_payment_methods`
  allowlist + `sale_payments.method_id` + seeds (`Cash,Transfer,Debit,CreditCard,QR`;
  sensible combos `Caja→Cash`, `Banco→Transfer,Debit,CreditCard`, `MP→QR,Transfer`
  applied where those accounts exist, plus `ensure_defaults_for_account` helper)
- `20240101000013_create_suppliers.sql` — `suppliers` (UNIQUE name, index on `is_active`)
- `20240101000014_create_product_supplier_costs.sql` — `product_supplier_costs`
  (UNIQUE product+supplier, one-preferred-per-product partial index)
- `20240101000015_create_purchases.sql` — `purchases` (UNIQUE purchase_number NULL-distinct,
  CHECKs, indexes on status/supplier/purchase_date)
- `20240101000016_create_purchase_lines.sql` — `purchase_lines` (CASCADE purchase, RESTRICT product)
- `20240101000017_create_purchase_payments.sql` — `purchase_payments` (CASCADE purchase,
  RESTRICT account/method)
- `20240101000018_expand_stock_reason_purchase_return.sql` — adds `Purchase-return` reason
- `20240101000019_link_payments_to_transactions.sql` — `transactions.reference`
  (backfilled only from descriptions that exactly match the document number
  shape) plus `sale_payments` / `purchase_payments` `transaction_id` and
  `refund_transaction_id` FKs to `transactions(id)` RESTRICT

## REST API

Base: `http://localhost:3000`

```bash
# Accounts
curl http://localhost:3000/api/accounts
# -> { "accounts": [{id,name,balance,cached_balance,created_at}], "total_balance":"123.45" }

curl -X POST http://localhost:3000/api/accounts \
  -H "Content-Type: application/json" \
  -d '{"name":"Cash"}'
# also: "Bank", "MercadoPago" …

curl http://localhost:3000/api/accounts/1
# -> { id,name,balance,created_at, transactions: [...] }

# Payment methods allowlist (explicit; an empty list is rejected)
curl http://localhost:3000/api/accounts/1/payment-methods
# -> { account_id, allowed_method_ids:[1], methods:[{id,name,is_active,allowed}, ...] }

curl -X PUT http://localhost:3000/api/accounts/1/payment-methods \
  -H "Content-Type: application/json" \
  -d '{"method_ids":[2,3]}'
# replaces the account's set (no merge); unknown id => 404, [] => 400
# Accounts without methods cannot record payments; the UI flags them.

# Transactions
curl "http://localhost:3000/api/transactions?account_id=1&from=2024-01-01&to=2024-12-31"

curl -X POST http://localhost:3000/api/transactions \
  -H "Content-Type: application/json" \
  -d '{"account_id":1,"type":"Income","amount":"1000.50","description":"Salary","date":"2024-01-15"}'
# response includes "reference": null; add "reference":"opaque-id" to stamp
# an opaque source id (documents use their YYYY-SALE-NNNNNN / YYYY-PURCH-NNNNNN)

curl -X PUT http://localhost:3000/api/transactions/1 \
  -H "Content-Type: application/json" \
  -d '{"amount":"1200.00","description":"Updated"}'

curl -X DELETE http://localhost:3000/api/transactions/1
```

```bash
# Categories
curl http://localhost:3000/api/categories
curl -X POST http://localhost:3000/api/categories \
  -H "Content-Type: application/json" \
  -d '{"name":"Beverages"}'
# -> 201 { id,name,parent_id,created_at }

# Products
curl "http://localhost:3000/api/products?category_id=1"
curl -X POST http://localhost:3000/api/products \
  -H "Content-Type: application/json" \
  -d '{"sku":"YERBA-500","name":"Yerba 500g","kind":"Product","unit":"un","sale_price":"1200.50","cost_price":"800","track_stock":true,"min_stock":"5","max_stock":"50"}'
# -> 201 product; duplicate sku => 409

curl http://localhost:3000/api/products/1
curl http://localhost:3000/api/products/1/stock
# -> { product, stock:"10", suggested:"40" } (suggested = max - stock when stock <= min)

curl -X POST http://localhost:3000/api/products/1/barcodes \
  -H "Content-Type: application/json" \
  -d '{"code":"7790001"}'
# -> 201; duplicate code across products => 409

# Stock movements
curl "http://localhost:3000/api/stock-movements?product_id=1"
curl -X POST http://localhost:3000/api/stock-movements \
  -H "Content-Type: application/json" \
  -d '{"product_id":1,"qty":"20","type":"In","reason":"Initial","date":"2024-01-15"}'
# type In=+qty, Out=-qty, Adjust=signed delta; Service/untracked => 400

curl http://localhost:3000/api/low-stock
# -> { low_stock: [{ product, stock, suggested }] }
curl http://localhost:3000/api/negative-stock
# -> { negative_stock: [...] }
```

```bash
# Sales (Draft -> Confirmed -> Cancelled orchestrator)
curl -X POST http://localhost:3000/api/sales \
  -H "Content-Type: application/json" \
  -d '{"customer_name":"Ana","payment_type":"Cash","sale_date":"2024-05-02"}'
# -> 201 sale detail (sale_number null while Draft; Credit needs due_date)

curl -X POST http://localhost:3000/api/sales/1/lines \
  -H "Content-Type: application/json" \
  -d '{"product_id":1,"qty":"3"}'
# -> 201 line (unit_price defaults to list price; unknown product => 404, qty <= 0 => 400)

curl -X PUT http://localhost:3000/api/sales/1 \
  -H "Content-Type: application/json" \
  -d '{"customer_name":"Ana Gomez"}'
# -> 200 detail (Draft only; edit Confirmed => 400)

curl -X POST http://localhost:3000/api/sales/1/confirm \
  -H "Content-Type: application/json" \
  -d '{"account_id":1,"method_id":1}'
# -> 200 detail with sale_number "2024-SALE-000001"; deducts stock,
#    Cash posts 1 Income with method. Credit: send {} (no account/method),
#    posts nothing, due = total. Disallowed (account,method) => 400, no touch.
#    Double confirm => 400.

curl -X POST http://localhost:3000/api/sales/1/payments \
  -H "Content-Type: application/json" \
  -d '{"account_id":1,"method_id":1,"amount":"15","date":"2024-05-10"}'
# -> 201 payment + 1 Income (Credit sales; N payments, mixed accounts/methods,
#    sum <= total; disallowed pair => 400, no finance touch; overpay => 400)

curl -X PUT http://localhost:3000/api/sales/lines/1 \
  -H "Content-Type: application/json" \
  -d '{"qty":"2","unit_price":"10"}'
curl -X DELETE http://localhost:3000/api/sales/lines/1
# -> Draft line edit / remove (204)

curl -X POST http://localhost:3000/api/sales/1/cancel \
  -H "Content-Type: application/json" \
  -d '{"reason":"customer return"}'
# -> 200 Cancelled; Confirmed re-enters stock + Expense refunds
#    (overdraft guard applies); Draft cancel is a no-op (number stays null).

curl http://localhost:3000/api/sales
# -> { sales: [detail with total/paid/due/payment_status] }
curl http://localhost:3000/api/sales/debt
# -> { debt: [Confirmed sales with due > 0] }
```

```bash
# Suppliers + product/supplier costs (M3)
curl -X POST http://localhost:3000/api/suppliers \
  -H "Content-Type: application/json" \
  -d '{"name":"Distribuidora Sur","phone":"11 5555-5555"}'
# -> 201 supplier (name trimmed; duplicate => 409)

curl http://localhost:3000/api/suppliers
curl http://localhost:3000/api/suppliers/1
curl -X PUT http://localhost:3000/api/suppliers/1 \
  -H "Content-Type: application/json" \
  -d '{"phone":"11 4444-4444"}'
curl -X POST http://localhost:3000/api/suppliers/1/deactivate   # 200 is_active false
curl -X POST http://localhost:3000/api/suppliers/1/activate
curl -X DELETE http://localhost:3000/api/suppliers/1
# -> 204 when unused; 400 when cost rows/purchases exist (deactivate instead)

curl -X POST http://localhost:3000/api/product-supplier-costs \
  -H "Content-Type: application/json" \
  -d '{"product_id":1,"supplier_id":1,"cost":"800","date":"2024-05-01"}'
# -> 201 satellite row; a later different cost shifts current -> previous

curl "http://localhost:3000/api/product-supplier-costs?product_id=1"
# -> { costs: [current_cost, previous_cost, dates, is_preferred] }

# Purchases (Draft -> Confirmed -> Cancelled orchestrator)
curl -X POST http://localhost:3000/api/purchases \
  -H "Content-Type: application/json" \
  -d '{"supplier_id":1,"payment_type":"Cash","purchase_date":"2024-05-02"}'
# -> 201 purchase detail (purchase_number null while Draft; Credit needs due_date)

curl -X POST http://localhost:3000/api/purchases/1/lines \
  -H "Content-Type: application/json" \
  -d '{"product_id":1,"qty":"10"}'
# -> 201 line (unit_cost defaults to products.cost_price; unknown product => 404,
#    qty <= 0 or unit_cost < 0 => 400, repeated product => 400)

curl -X PUT http://localhost:3000/api/purchases/1 \
  -H "Content-Type: application/json" \
  -d '{"notes":"pedido semanal"}'
# -> 200 detail (Draft only; edit Confirmed => 400)

curl -X POST http://localhost:3000/api/purchases/1/confirm \
  -H "Content-Type: application/json" \
  -d '{"account_id":1,"method_id":1}'
# -> 200 detail with purchase_number "2024-PURCH-000001"; receives stock (In,
#    reason Purchase) and updates the satellite cost. Credit: send {} (no
#    account/method), due = total, no Expense. Disallowed (account,method) => 400
#    with no stock/finance touch. Double confirm => 400.

curl -X POST http://localhost:3000/api/purchases/1/payments \
  -H "Content-Type: application/json" \
  -d '{"account_id":1,"method_id":1,"amount":"500","date":"2024-05-10"}'
# -> 201 payment + 1 Expense (Credit purchases; sum <= total; overpay => 400;
#    disallowed pair => 400 with no finance touch)

curl -X PUT http://localhost:3000/api/purchases/lines/1 \
  -H "Content-Type: application/json" \
  -d '{"qty":"8","unit_cost":"810"}'
curl -X DELETE http://localhost:3000/api/purchases/lines/1   # -> 204

curl -X POST http://localhost:3000/api/purchases/1/cancel \
  -H "Content-Type: application/json" \
  -d '{"reason":"wrong order"}'
# -> 200 Cancelled; Confirmed returns stock (Out, reason Purchase-return) and posts
#    Income refunds per payment (no balance guard); Draft cancel is a discard
#    (purchase_number stays null).

curl http://localhost:3000/api/purchases
# -> { purchases: [detail with total/paid/due/payment_status] }
curl http://localhost:3000/api/purchases/suggestions
# -> { suggestions: [low-stock rows with supplier/qty/cost/subtotal],
#      without_supplier: [low-stock products with no satellite row] }
```

Error format:

```json
{ "error": "amount must be > 0" }
```

- `400` validation (empty name, amount <=0, from>to, insufficient funds, sale/purchase guards below)
- `404` not found (incl. unknown product/account/supplier on purchase and sale flows)
- `409` duplicate account name (UNIQUE), duplicate sku/barcode/receipt_no/sale_number,
  duplicate supplier name, duplicate preferred cost row

Money is `rust_decimal::Decimal` serialized as **string** (`serde-with-str`) to avoid float rounding. Never uses `f32/f64`.

## Web UI

`GET /` — dashboard:

- Total balance (derived)
- Account list with balances (HTMX `GET /web/accounts`, OOB swap)
- Recent transactions filtered by account/date (`GET /web/transactions`)
- Forms:
  - Create account: `POST /web/accounts` (HTMX, `hx-post`, no reload)
  - Create transaction: `POST /web/transactions` (HTMX)

`GET /accounts/:id` — detail with history + delete buttons (`hx-delete`).

`GET /products` — inventory:

- Product list with derived stock + low/negative badges (HTMX `GET /web/products`, filter by `category_id`)
- Low-stock list with reorder suggestion (HTMX `GET /web/low-stock`, also `GET /web/negative-stock`)
- Forms:
  - Create category: `POST /web/categories` (HTMX)
  - Create product: `POST /web/products` (HTMX)
  - Record movement: `POST /web/stock-movements` (HTMX)

`GET /sales` — sales:

- Sale list with status/debt badges (HTMX `GET /web/sales`)
- Sale detail with lines + payments (HTMX `GET /web/sales/:id`, line delete via `hx-delete` in Draft)
- Outstanding debt — Confirmed sales with due > 0 (HTMX `GET /web/sales/debt`)
- Forms:
  - New sale Draft: `POST /web/sales` (HTMX)
  - Add line: `POST /web/sales/:id/lines` (HTMX)
  - Confirm: `POST /web/sales/:id/confirm` (HTMX)
  - Record payment: `POST /web/sales/:id/payments` (HTMX)
  - Cancel: `POST /web/sales/:id/cancel` (HTMX)

`GET /purchases` — purchases (M3):

- Purchase list with status + payable badges (HTMX `GET /web/purchases`, derived total/paid/due)
- Sugerido panel rendering the suggestion with a `→ Draft` seed button per row
  (HTMX `GET /web/purchases/suggestions`, seed via `POST /web/purchases/from-suggestion`)
- Purchase detail with lines and payments; the Draft line editor saves qty/unit cost in
  place and removes lines (HTMX `GET /web/purchases/:id`, POST/DELETE
  `/web/purchases/:id/lines/:line_id`)
- Forms:
  - New purchase Draft: `POST /web/purchases` (HTMX)
  - Add line: `POST /web/purchases/lines` (HTMX)
  - Confirm: `POST /web/purchases/confirm` (HTMX)
  - Record payment: `POST /web/purchases/payments` (HTMX)
  - Cancel: `POST /web/purchases/cancel` (HTMX)

`GET /suppliers` — suppliers + cost satellite (M3):

- Supplier list with active/inactive badge and every satellite cost row (derived
  raised/lowered alert, preferred marker) (HTMX `GET /web/suppliers`)
- Forms:
  - Create supplier: `POST /web/suppliers` (HTMX)
  - Edit supplier: `POST /web/suppliers/edit` (HTMX)
  - Activate/deactivate: `POST /web/suppliers/:id/activate|deactivate` (HTMX)
  - Delete supplier: `DELETE /web/suppliers/:id` (HTMX, RESTRICT-aware)
  - Record product cost: `POST /web/supplier-costs` (HTMX)

All forms use HTMX; server returns HTML fragments (`partials/*`) and `HX-Trigger` events for refresh. HTMX 1.9.12 is served locally from `/static/htmx.min.js` (no CDN).

Navigation: the header links Dashboard, Products, Sales, Purchases and Suppliers; page-level links reach the detail/back views.

## Styles & local assets

The UI is styled with Tailwind CSS v4 utilities. The source of truth is `assets/tailwind.css`; the compiled stylesheet is committed at `static/tailwind.css`. Both files under `static/` are served by Axum at `/static/*` (`tower-http` `ServeDir` nested in `routes::router`), so the app has no CDN dependencies:

- `static/tailwind.css` — compiled Tailwind stylesheet
- `static/htmx.min.js` — HTMX 1.9.12

Rebuild the CSS after changing templates or the entrypoint. The standalone Tailwind CLI is a dev-time tool only; it is not a Rust dependency and is not committed:

```bash
# Standalone CLI in PATH
scripts/build-css.sh

# Or with an explicit binary location
TAILWINDCSS=~/.local/bin/tailwindcss scripts/build-css.sh

# Or directly
tailwindcss --input assets/tailwind.css --output static/tailwind.css --minify
```

The entrypoint imports Tailwind, scans only `templates/` (`@source`), and defines the dark palette as `@theme` tokens (`bg`, `surface`, `card`, `border`, `text`, `muted`, `accent`, `accent2`, `danger`, `income`, `expense`). Commit the regenerated `static/tailwind.css` together with the template change so `cargo run` keeps working without the CLI installed.

## Configuration

| Env | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | `sqlite://roya.db` | SQLite file (or `postgres://...`) |
| `ALLOW_NEGATIVE_BALANCE` | `false` | If `false`, Expense that would make balance negative is rejected (also on edit/delete of Income) |
| `ALLOW_NEGATIVE_STOCK` | `true` | If `false`, Out that would make stock negative is rejected (400, stock unchanged); if `true`, Out succeeds (201) and product appears in negative list |
| `PORT` | `3000` | HTTP port |
| `RUST_LOG` | `info` | tracing filter |

## Validation & Balance Rules

- `Transaction.reference` is opaque and nullable: document flows stamp the
  sale/purchase number, manual transactions leave it NULL. The payment →
  transaction FK is RESTRICT, so a linked movement cannot be deleted while a
  payment references it; cancelling a document links the refund transaction in
  `refund_transaction_id` without touching the original `transaction_id`.
- `Account.name` trimmed, non-empty, ≤64, UNIQUE.
- `Transaction.amount` parsed as `Decimal`, must be `> 0`. Stored as **TEXT** in SQLite to preserve precision (SQLite has no native Decimal; `NUMERIC` affinity would coerce to REAL and lose digits — see `sqlx-sqlite` docs). Repositories encode/decode via string and sum in Rust to keep Decimal exactness.
- `Transaction.date` = `NaiveDate` (YYYY-MM-DD).
- Negative guard (`ALLOW_NEGATIVE_BALANCE=false`):
  - `create Expense`: `projected = current_balance - amount` must be `>=0`.
  - `update`: recomputes `current - old_signed + new_signed`.
  - `delete Income`: `projected = current - amount` must be `>=0`.
- `Sale` rules: Draft editable (lines/customer/dates); Confirmed/Cancelled immutable
  except Cancel. `sale_number` immutable once set (`YYYY-SALE-NNNNNN`), NULL only in
  Draft/Cancelled-from-Draft. Credit requires `due_date >= sale_date`, Cash forbids it.
  `qty > 0`, `unit_price >= 0`, payments carry `account_id + method_id` (N per sale,
  mixed, sum ≤ total), reject overpay (`paid + amount <= total`) and disallowed
  `(account,method)` (400, no stock/sequence/finance touch).
  No new env vars for sales (reuses `ALLOW_NEGATIVE_BALANCE` / `ALLOW_NEGATIVE_STOCK`).
- `PaymentMethod` rules: `name` UNIQUE, `is_active` 0/1; allowlist
  `PK(account_id, method_id)` RESTRICT both; `sale_payments.method_id` RESTRICT
  NOT NULL; unknown method => 404, inactive/disallowed => 400.
  - Balance read path: `SELECT kind, amount FROM transactions WHERE account_id=?` summed in Rust (not `SUM()` which would cast TEXT→REAL).
- `Supplier` rules: `name` trimmed, non-empty, ≤128, UNIQUE; `phone` ≤32 and `notes`
  ≤512 (empty clears to NULL); delete blocked (400) when the supplier has cost rows or
  purchases (RESTRICT) — deactivate instead.
- `ProductSupplierCost` rules (frozen option A): `current_cost >= 0`; a later cost that
  differs shifts current → previous with both dates, an equal cost only refreshes
  `current_cost_updated_at`; the cost date can never precede the current cost date
  (400); at most one `is_preferred=1` per product; the raised/lowered alert is derived
  from `previous_cost` vs `current_cost` and never stored. `products.cost_price` is not
  written by purchases: the satellite wins when rows exist, otherwise the column.
- `Purchase` rules: Draft editable (lines/header); Confirmed/Cancelled immutable except
  Cancel. `purchase_number` immutable once set (`YYYY-PURCH-NNNNNN`), NULL only in
  Draft/Cancelled-from-Draft. Credit requires `due_date >= purchase_date`, Cash forbids
  it. `qty > 0`, `unit_cost >= 0`, no duplicated product per purchase, payments carry
  `account_id + method_id` (mixed, sum ≤ total), reject overpay and disallowed
  `(account,method)` (400, no stock/sequence/finance touch). Confirm updates the
  satellite per line; cancelling a Confirmed purchase returns stock and refunds paid
  amounts as Income. No new env vars for purchases (reuses `ALLOW_NEGATIVE_BALANCE` /
  `ALLOW_NEGATIVE_STOCK`).

## SQLite → Postgres Migration (without rewriting logic)

The codebase is designed to migrate via **trait swap**. Only `repositories/` and `db.rs` change; `services/` and `routes/` stay untouched.

**1. Cargo.toml**

```toml
# From
sqlx = { version = "0.9", features = ["runtime-tokio", "sqlite", "chrono", "rust_decimal"] }
# To
sqlx = { version = "0.9", features = ["runtime-tokio", "postgres", "sqlite", "chrono", "rust_decimal", "migrate"] }
# Optional: keep sqlite for tests/dev
```

`rust_decimal` stays `serde-with-str` only; `sqlx` with `rust_decimal` feature provides `Decimal` impl for Postgres (`NUMERIC` → Decimal natively, no TEXT hack needed).

**2. DATABASE_URL**

```bash
DATABASE_URL=postgres://user:pass@localhost:5432/roya
# .env
```

**3. Migrations**

Postgres uses real `NUMERIC(20,2)` / `DECIMAL` and `SERIAL`/`BIGSERIAL`:

```sql
-- migrations/*_create_accounts_pg.sql
CREATE TABLE accounts (
  id BIGSERIAL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  cached_balance NUMERIC(20,2) NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE transactions (
  id BIGSERIAL PRIMARY KEY,
  account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('Income','Expense')),
  amount NUMERIC(20,2) NOT NULL CHECK (amount > 0),
  description TEXT NOT NULL DEFAULT '',
  date DATE NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Run:

```bash
DATABASE_URL=postgres://... sqlx migrate run
# or let embedded migrate run at startup (same macro works for Postgres)
```

**4. db.rs**

```rust
// Before: SqlitePool
use sqlx::postgres::{PgPool, PgPoolOptions};

pub async fn create_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new().max_connections(5).connect(database_url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
```

**5. Repositories**

Implement the same traits for Postgres:

```rust
pub struct PgAccountRepository { pub pool: PgPool }
#[async_trait]
impl AccountRepository for PgAccountRepository { /* use $1, $2 Postgres binds + NUMERIC directly as Decimal */ }

pub struct PgTransactionRepository { pub pool: PgPool }
#[async_trait]
impl TransactionRepository for PgTransactionRepository { /* same */ }
```

Then in `routes::AppState::new` swap `Sqlite*` for `Pg*`:

```rust
let acc_repo = PgAccountRepository::new(pool.clone());
let tx_repo = PgTransactionRepository::new(pool.clone());
```

No change to `AccountService`/`TransactionService` or handlers — they depend only on the traits.

**Why TEXT for SQLite?** SQLite’s `NUMERIC` affinity silently converts `TEXT '1.2345678901234567890'` → `REAL 1.23456789012346` (15 digits), which would defeat `rust_decimal`. So SQLite stores `amount`/`cached_balance` as `TEXT` and сумме в Rust. Postgres’s `NUMERIC` is exact, so the trait impl can bind `Decimal` natively and use `SUM()` in SQL if desired.

## Code in English, Errors

- All code/comments in English.
- `thiserror` + `IntoResponse`: `AppError::NotFound / Validation / Conflict / Database` map to 404/400/409/500 with `{error: ...}`.

## Project Structure (as requested)

```
src/main.rs
src/models.rs
src/db.rs
src/error.rs
src/services/account.rs
src/services/transaction.rs
src/services/inventory.rs
src/services/sales.rs      — Draft/Confirm/Pay/Cancel orchestrator (calls Inventory + Transaction services, never SQLs their tables)
src/services/suppliers.rs  — supplier CRUD + product/supplier satellite cost rule
src/services/purchases.rs  — Draft/Confirm/Pay/Cancel + suggestion builder (orchestrates stock, finance, satellite)
src/repositories/account_repo.rs
src/repositories/transaction_repo.rs
src/repositories/category_repo.rs
src/repositories/product_repo.rs
src/repositories/barcode_repo.rs
src/repositories/stock_repo.rs
src/repositories/sale_repo.rs          — Sale/SaleLine/SalePayment SQLite impl
src/repositories/supplier_repo.rs      — Supplier SQLite impl (RESTRICT-aware delete)
src/repositories/product_supplier_cost_repo.rs — satellite cost SQLite impl
src/repositories/purchase_repo.rs      — Purchase/PurchaseLine/PurchasePayment SQLite impl
src/repositories/doc_sequence_repo.rs  — atomic YYYY-SALE-NNNNNN / YYYY-PURCH-NNNNNN numbering
src/routes/api.rs
src/routes/web.rs
src/routes/inventory_api.rs
src/routes/inventory_web.rs
src/routes/sales_api.rs    — REST /api/sales, lines, payments, confirm/cancel, debt
src/routes/sales_web.rs    — Web /sales Askama + HTMX
src/routes/purchases_api.rs — REST /api/suppliers, /api/product-supplier-costs, /api/purchases
src/routes/purchases_web.rs — Web /purchases Askama + HTMX (incl. Sugerido)
src/routes/suppliers_web.rs — Web /suppliers Askama + HTMX
src/routes/mod.rs
templates/base.html
templates/dashboard.html
templates/account_detail.html
templates/products.html
templates/sales.html
templates/purchases.html
templates/suppliers.html
templates/partials/*.html  — incl. sale_list.html, sale_detail.html, purchase_list.html,
                             purchase_detail.html, supplier_list.html, suggestion_list.html
migrations/*.sql
assets/tailwind.css        — Tailwind v4 entrypoint (@source templates/, @theme palette)
static/tailwind.css        — compiled stylesheet (committed; rebuild via scripts/build-css.sh)
static/htmx.min.js         — HTMX 1.9.12 served locally (no CDN)
scripts/build-css.sh       — regenerates static/tailwind.css with the standalone CLI
```

`src/templates/` placeholder exists for spec compliance; Askama loads from `templates/` at crate root (standard).

## No Heavy ORM / No Docker / No Auth

- No Diesel/SeaORM.
- No microservices, no mandatory Docker (just `cargo run`).
- No auth (local single-user).
- `tower-http` trace + cors + static file serving (`ServeDir`).

## Tests (manual)

```bash
# Run server then in another shell:
curl -X POST http://localhost:3000/api/accounts -H "Content-Type: application/json" -d '{"name":"Cash"}'
curl -X POST http://localhost:3000/api/transactions -H "Content-Type: application/json" -d '{"account_id":1,"type":"Income","amount":"500","description":"test","date":"2024-01-01"}'
curl "http://localhost:3000/api/transactions?account_id=1"
```

## Tests (Rust smoke suite)

`cargo test` runs the unit suites plus the HTTP smoke suite in
`src/smoke_tests.rs`. The smoke tests build the app exactly like `main` does
(in-memory SQLite with the real migrations + `AppState::new`) and drive the full
router, form extraction and Askama rendering.

- **Business flows** (one per test): account with payment methods, tracked
  product and supplier cost; cash sale confirm (stock down, exactly one linked
  Income); credit sale pay/overpay/cancel (stock re-entered, linked refund, the
  original transaction link kept); purchase from the suggestion endpoint
  (confirm Cash, linked Expense, cancel reversal); the no-methods payment guard;
  and the account-balance + payment-traceability invariants (every payment's
  `transaction_id` and, when present, its `refund_transaction_id` resolve to a
  real transaction whose `reference` is the document number; a refund must also
  reverse its own payment: same account, opposite kind and equal amount; and
  globally no transaction id may be claimed by two payments, so equal-amount
  cross-payment swaps fail even though every fact matches. That ownership rule
  is only reachable through direct database tampering: the application always
  creates a fresh refund per payment and no route accepts
  `refund_transaction_id`).
- **Generic form-wiring guard**: for the seeded `/`, `/accounts/{id}`,
  `/products`, `/sales`, `/purchases` and `/suppliers` pages (plus the sale and
  purchase detail fragments) it extracts every `hx-get`, `hx-post`, `hx-put`,
  `hx-patch` and `hx-delete` target with the HTTP verb htmx will send, the
  native `action`/`onsubmit` wiring of rendered forms, and the application URLs
  written inside `hx-on` bodies and inline scripts (verb from
  `htmx.ajax('POST', ...)`, GET by default) after decoding HTML entities and
  unwrapping single- or double-quoted attributes and plain backticks, then
  probes each target against the router with that real verb. Selectors (`#...`),
  event names and non-path string literals are ignored. A URL built dynamically
  (concatenation or `${}` template interpolation around an application path) is
  rejected with a message that asks for a plain quoted literal, so it cannot
  hide from static verification.
- **Routing oracle and verb check**: unmatched paths answer
  `404 {"error":"route not found"}` and a registered path probed with the wrong
  verb answers `405`; both fail the guard. A dead target masked by a path-param
  route (`hx-post="/web/sales/does-not-exist"`) and a GET-only path used as an
  `hx-post` target are caught. Handler-level 404s keep their own message, and
  the guard probes against its own freshly seeded app instance so the real
  handlers it may run cannot mutate the apps used by the flow assertions.
- **Typed-id shells**: `/`, `/sales` and `/purchases` are the pages where the
  user types the id into the form, so a concrete numeric path segment in a
  form-bound target is rejected (`/web/sales/1/confirm`) while data-bound record
  links such as the list View buttons stay valid. A `:` is a placeholder marker
  only in the path, so `datetime` query values pass.
- **Native forms**: a `this.action=` rewrite inside `onsubmit` fails the guard
  (htmx ignores the form action property), and native `action=` targets are
  probed with their form method like any other target.

## License

MIT
