# Roya — Local Business Manager

Local business manager for a polirrubro (goods and services): sales of products and services, stock, purchases from suppliers with per-supplier cost history, customers with credit and receivable ageing, and the money behind all of it. One SQLite file, a REST API and an HTMX web interface, no cloud and no internet required.

Stack: **Rust + Axum 0.8.9 + Tokio + SQLx 0.9 (SQLite → Postgres) + Askama + HTMX 1.9.12 + Tailwind CSS 4.3.3 + rust_decimal + chrono**.

The front end is server-rendered: **HTMX 1.9.12** and the compiled **Tailwind CSS 4.3.3** stylesheet are vendored under `static/` and served by Axum, so there is no CDN, no npm and no runtime network dependency. See [Styles & local assets](#styles--local-assets).

## Features

- **Identity & RBAC (M5)** — every route is behind a deny-by-default session gate: users with
  argon2id credentials, revocable sessions (sha256 token hash only, SQL-decided validity, absolute
  TTL with a 30-minute sliding renewal), a permission catalog seeded by migration and compared to
  the compiled one by a drift test, roles whose permission matrix is edited from the interface
  (`/users`, `/roles`), a protected `admin` role and last-administrator guarantees enforced by
  database triggers, the navigation rendering only the entries the principal may read, and role
  grants recording who granted them (`user_roles.granted_by`/`granted_at`). Every department route
  declares the permission its action needs; the kernel answers — see
  `openspec/specs/identity/spec.md` for the complete route → permission table.
- **Actor audit (M5 Phase B)** — every mutation of the business tables and the identity tables
  records who made it happen: `created_by` (`NOT NULL`, FK to `users`, `ON DELETE RESTRICT`) and
  `updated_by` (nullable) on `accounts`, `transactions`, `payment_methods`, `categories`,
  `products`, `stock_movements`, `sales`, `sale_payments`, `customer_receipts`, `customers`,
  `purchases`, `purchase_payments`, `suppliers`, `product_supplier_costs`, `roles` and
  `permissions`; `users` carries nullable self-referencing columns where NULL means "the system"
  (the sentinel and the bootstrap administrator are the system's work). The actor comes from the
  authenticated principal, never from anything the request supplies, and a document created inside
  a flow (a sale's payment, a purchase's cost row, a confirm's stock movement) carries the flow's
  request actor. Rows that predate the audit are attributed to the inactive, roleless sentinel
  account `sistema` ("Sistema (anterior al registro)") — not to a person, because attributing
  pre-audit rows to one would invent history. The interface shows the actor as a display name,
  never an id ("Registrado por" / "Actualizado por" in the detail views, "el sistema" for the
  NULL `users` columns, and the grant trail "«rol»: otorgado por «nombre» el «fecha»" on the
  users screen) — see `openspec/specs/identity/spec.md` ("The actor audit") for the rules.
- **Documents index (`/documents`)** — one screen over the documents the shop produces: sales,
  sale payments, purchases, purchase payments, stock movements and customer receipts, newest first,
  with a text search (document number/reference and counterpart name) and filters by type, acting
  user and inclusive date range. The index itself writes nothing; every row opens the document page
  that already owns it, and the drawer additionally re-presents each document's own real actions
  (draft delete with `sales.create`/`purchases.create`, annul/discard with the cancel codes) with a
  server-computed impact preview — see the `GET /documents` block below. Visibility is per type and
  uses the existing catalog — `sales.read` opens the
  sale documents, `purchases.read` the purchases, `inventory.read` the stock movements and
  `customers.read` the collection receipts; any ONE of the four opens the screen, and the page
  narrows its content to the tiers the principal holds (no new permission, no migration). The feed
  shows the newest 200 documents and says so when it cut the history. Movements of cash
  (`transactions`) are deliberately out of scope: the index answers "which document", the ledger
  answers "which money".
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
  - Every sale carries a mandatory `customer_id` (`NOT NULL`, indexed, RESTRICT).
    Cash sales default to the seeded walk-in (`Consumidor final`) in the sale
    form; `customer_name` is frozen as a snapshot of the customer's name at
    creation time, so correcting a customer never rewrites history.
  - Draft creates lines with no stock/finance side effects.
  - Confirm assigns `YYYY-SALE-NNNNNN`, deducts stock (`Out`, reason `Sale`),
    Cash posts 1 Income with method, Credit opens a receivable (due = total).
  - Credit rejects the walk-in customer (400): a receivable needs a real
    customer. When the customer has a `credit_limit` and
    `ENFORCE_CREDIT_LIMIT=true` (the default), confirming a credit sale whose
    projected debt (`customer debt + total`) exceeds the limit returns 400 with
    the projected figure; a null limit is never checked. The debt is derived
    from confirmed credit sales minus their payments, so cancelled sales stop
    counting.
  - For a credit sale without `due_date`, the due date defaults to
    `sale_date + customer.payment_days`; without a term on the customer it is
    required (400). An explicit date always wins.
  - Payments name only the `method_id` (N per sale, mixed methods, sum ≤
    total); each posts 1 Income; overpay ⇒ 400; Paid when due = 0. The account
    is derived from the method's owner, so an invalid combination is impossible
    by construction; an unassigned or inactive method ⇒ 400 before any
    stock/sequence/finance touch.
  - Cancel of Confirmed re-enters stock (`In`, reason `Sale-return`) and posts
    Expense refunds, guarded by `ALLOW_NEGATIVE_BALANCE`.
  - `sale_number` UNIQUE, immutable, NULL only in Draft/Cancelled-from-Draft.
  - Finance/stock rows are written only via services, reference = `sale_number`;
    each payment stores the id of the Income it created.
- **Customers (M4)** — `customers(id, name, phone, address, tax_id, notes,
  is_walkin, is_active, credit_limit NULL = no limit, payment_days NULL = no term,
  created_at, updated_at)`, seeded with the walk-in `Consumidor final`
  (`is_walkin = 1`, never deactivatable, never deletable, never duplicated). Names
  are **not unique**: creating a duplicate is accepted and the existing matches are
  returned so the interface warns without blocking. The receivable is derived from
  sales, never stored: `balance = confirmed credit sales − their payments`,
  `ageing` from `due_date` (current / 1-30 / 31-60 / 61+ days overdue) and
  `over_limit` when a set limit is exceeded. REST: `GET/POST /api/customers`,
  `GET/PUT/DELETE /api/customers/:id`,
  `POST /api/customers/:id/activate|deactivate`,
  `GET /api/customers/:id/statement`, `GET /api/customers/ageing`.
- **Customer receipts (M4)** — `customer_receipts(id, customer_id, account_id,
  method_id, date, notes, created_at)` groups the payments of one handover of
  money (the stored `account_id` is derived from the method at collect time).
  Collecting names only the method and applies the amount to the customer's
  confirmed credit sales oldest debt first, creates one receipt and one linked
  payment per covered sale, and every grouped payment still posts its own Income
  with `reference = sale_number`. The receipt stores no total: it is derived as
  `SUM(allocations)`, so an interrupted collection can never claim more than it
  applied. More than the outstanding debt ⇒ 400; an unassigned/inactive method
  ⇒ 400; both leave no side effect. No route accepts a receipt id: the
  same-customer rule is enforced by construction in `collect`, and the database
  triggers stay the backstop. REST: `GET/POST /api/customer-receipts`,
  `GET /api/customer-receipts/:id` (list requires `?customer_id=`).
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
    mixed methods, sum ≤ total; Paid when due = 0; overpay ⇒ 400.
  - Supplier-level payment: one handover of money allocated oldest debt first
    (`due_date`, then `purchase_date`, then id) across the supplier's Confirmed
    credit purchases, one payment per covered purchase, each posting its own
    Expense. Suppliers have no grouping receipt document, so there is no receipt
    id: the result is the created payments. More than the outstanding debt ⇒ 400
    naming both figures; an unassigned/inactive method ⇒ 400; both leave no side
    effect. Web form in the supplier drawer; REST: `POST /api/supplier-payments`.
  - Cancel of Confirmed returns stock (`Out`, reason `Purchase-return`, the M1 CHECK
    expansion) and posts Income refunds per payment; a refund is money entering, so the
    balance guard never blocks it. Draft cancel is a discard with no side effects.
  - The method's owning account is resolved (400 when unassigned/inactive)
    before any stock/sequence/finance touch; `purchase_number` UNIQUE, immutable,
    NULL only while Draft (or cancelled before ever being confirmed).
- **Sugerido (purchase suggestion)** — low-stock tracked products with suggested
  qty = `max_stock − stock`, the chosen supplier (preferred satellite row, else cheapest
  current cost), satellite cost and subtotal. Products without a satellite row are
  returned in `without_supplier`, never silently dropped.
- **Payment methods (M0)** — `payment_methods(id, name, account_id NULL,
  is_active)` seeded `Cash, Transfer, Debit, CreditCard, QR` (no `Other`),
  unassigned until an account owns them. Each method belongs to at most one
  account (`UNIQUE(account_id, name)` lets two accounts each own a same-named
  method as separate rows); `sale_payments.method_id` RESTRICT NOT NULL.
  `sales` has no `account_id`. Payments name only the method and the account is
  derived from ownership, so every form is a single method select rendered
  `"Name — AccountName"`.
  Method assignment is **explicit configuration, never a silent default**: the
  dashboard create-account form ticks methods into the new account (unassigned
  ones are assigned, ones owned elsewhere are duplicated), the account detail
  page (`/accounts/:id`) assigns/unassigns its set, and REST uses
  `GET/PUT /api/accounts/:id/payment-methods` (unknown id ⇒ 404, methods owned
  by another account ⇒ 400; an empty list unassigns everything) plus the global
  `GET /api/payment-methods` catalog. Accounts with no methods are flagged on
  the dashboard list and detail page, and paying with an unassigned method fails
  with a 400 that names the fix.
  Migration 12 still seeds `Caja→Cash`, `Banco→Transfer,Debit,CreditCard`,
  `MP→QR,Transfer` for accounts that already exist, and
  `PaymentMethodService::ensure_defaults_for_account` keeps that mapping reusable
  (assigning free rows, duplicating owned names); no accounts are auto-created.
  Migration 24 converted the old M:N allowlist (`account_payment_methods`,
  dropped) to this 1:N ownership, splitting shared methods into one row per
  account and keeping every method id stable, so payment history is untouched.
  **Breaking change vs the pair API**: `POST /api/customer-receipts`, sale/purchase
  confirms and sale/purchase payments no longer accept `account_id`.

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
  security/      — the identity kernel: password.rs (argon2id), session.rs (cookie),
                   authz.rs (principal, permission catalog, Require<P>, nav view), guard.rs
                   (deny-by-default middleware), test_support.rs (the shared authenticated test helper)
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
# ENFORCE_CREDIT_LIMIT=true
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
- `20240101000020_create_customers.sql` — `customers` (indexes on `is_active` and
  `is_walkin`, guarded walk-in seed, triggers protecting the walk-in)
- `20240101000021_add_sales_customer.sql` — `sales.customer_id` NOT NULL (backfill
  to the walk-in, table rebuild) + index; `customer_name` stays the frozen snapshot
- `20240101000022_create_customer_receipts.sql` — `customer_receipts` (FKs RESTRICT,
  indexes on `customer_id` and `date`)
- `20240101000023_add_sale_payments_receipt.sql` — `sale_payments.receipt_id`
  (RESTRICT, indexed) + triggers refusing a payment grouped under another
  customer's receipt
- `20240101000024_payment_methods_single_account.sql` — `payment_methods.account_id`
  (NULL = unassigned, RESTRICT) + `UNIQUE(account_id, name)` replacing the dropped
  `account_payment_methods` allowlist; shared methods split into one row per account,
  orphans stay NULL, method ids stable (history untouched). Runs `-- no-transaction`
  with `PRAGMA foreign_keys=OFF` for the parent-table swap (a deferred violation from
  `DROP TABLE` cannot be healed before COMMIT).
- `20240101000025_create_identity_users.sql` — `users` (S1a identity kernel: the
  login key with a lowercase-COLLATE unique index, argon2id `password_hash` never
  returned by any read path, `is_active` deactivation, `must_change_password` flag,
  seeded `admin`)
- `20240101000026_create_identity_sessions.sql` — `sessions` (S1a: one row per login,
  only the sha256 digest of the cookie token is stored, validity decided in SQL,
  permanent revocation guarded by a trigger)
- `20240101000027_create_identity_rbac.sql` — `roles`, `permissions` (the 23-code seeded
  catalog), `role_permissions`, `user_roles` with the `granted_by`/`granted_at` grant trail;
  seeded `admin` (protected, holds the whole catalog) plus `vendedor`, `cajero`, `deposito`
  with their matrices, every insert guarded so re-running cannot duplicate
- `20240101000028_create_identity_guards.sql` — the lockout triggers: a protected role cannot be
  deleted, renamed or have its permission rows removed; the last active holder of the protected
  role cannot be deactivated or lose its grant
- `20240101000029_clarify_identity_permission_descriptions.sql` — corrected seeded descriptions
  for the two `identity.*` manage codes (they now say exactly what the gate allows; the AC12 drift
  test compares descriptions as well as codes)
- `20240101000030_add_audit_finance.sql` — the actor audit begins (Phase B): `created_by` NOT NULL
  + `updated_by` on `accounts`, `transactions`, `payment_methods`; creates the sentinel account
  `sistema` (inactive, roleless, deliberately malformed hash) and attributes every pre-existing row
  to it — an honest attribution, never a person's name on rows nobody's screen created
- `20240101000031_add_audit_inventory.sql` — audit columns on `categories`, `products`,
  `stock_movements`; reuses the sentinel (its own guarded insert is defensive only)
- `20240101000032_add_audit_sales_customers.sql` — audit columns on `sales`, `sale_payments`,
  `customer_receipts` and `customers`; `sale_lines` gains no columns (a line inherits its sale's
  actor), and a sale's payments carry the confirming request's actor
- `20240101000033_add_audit_purchases_suppliers.sql` — audit columns on `purchases`,
  `purchase_payments`, `suppliers`, `product_supplier_costs`; `purchase_lines` inherit, a line
  change stamps the draft's `updated_by`
- `20240101000034_add_audit_identity_tables.sql` — the audit reaches the identity tables:
  `roles`/`permissions` rebuilt with NOT NULL `created_by` (the seven identity guard triggers
  dropped and recreated byte-identically; `permissions.updated_by` has no runtime writer — the
  catalog is never written at runtime), `users` ALTERed with nullable self-referencing columns
  (NULL = the system, rendered "el sistema"), `role_permissions` inheriting the role's actor and
  the matrix edit stamping the role's `updated_by`

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

# Account methods (ownership; empty list unassigns everything)
curl http://localhost:3000/api/accounts/1/payment-methods
# -> { account_id, method_ids:[1], methods:[{id,name,account_id,is_active}, ...] }

curl -X PUT http://localhost:3000/api/accounts/1/payment-methods \
  -H "Content-Type: application/json" \
  -d '{"method_ids":[2,3]}'
# replaces the account's set (no merge, no stealing: unknown id => 404, methods
# owned by another account => 400, [] unassigns everything)
# Accounts without methods cannot record payments; the UI flags them.

curl http://localhost:3000/api/payment-methods
# -> { methods: [every method with its owning account] }

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
curl -X PUT http://localhost:3000/api/products/1 \
  -H "Content-Type: application/json" \
  -d '{"sale_price":"1300.00","location":null}'
# -> 200 product; omitted keys keep their value, an explicit null clears a field

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
  -d '{"customer_id":1,"payment_type":"Cash","sale_date":"2024-05-02"}'
# -> 201 sale detail (customer_id required: unknown => 404; customer_name is
#    snapshotted from the customer. sale_number null while Draft; for Credit
#    due_date is optional when the customer has a payment term.)

curl -X POST http://localhost:3000/api/sales/1/lines \
  -H "Content-Type: application/json" \
  -d '{"product_id":1,"qty":"3"}'
# -> 201 line (unit_price defaults to list price; unknown product => 404, qty <= 0 => 400)

curl -X PUT http://localhost:3000/api/sales/1 \
  -H "Content-Type: application/json" \
  -d '{"notes":"llamar antes de entregar"}'
# -> 200 detail (Draft only; the customer and name snapshot are immutable;
#    edit Confirmed => 400)

curl -X POST http://localhost:3000/api/sales/1/confirm \
  -H "Content-Type: application/json" \
  -d '{"method_id":1}'
# -> 200 detail with sale_number "2024-SALE-000001"; deducts stock,
#    Cash posts 1 Income with the method's account. Credit: send {} (no method),
#    posts nothing, due = total. Unassigned/inactive method => 400, no touch.
#    Double confirm => 400.

curl -X POST http://localhost:3000/api/sales/1/payments \
  -H "Content-Type: application/json" \
  -d '{"method_id":1,"amount":"15","date":"2024-05-10"}'
# -> 201 payment + 1 Income (Credit sales; N payments, mixed methods,
#    sum <= total; unassigned method => 400, no finance touch; overpay => 400)

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
# Customers (M4)
curl -X POST http://localhost:3000/api/customers \
  -H "Content-Type: application/json" \
  -d '{"name":"Ana Pérez","phone":"555-1234","credit_limit":"1000","payment_days":30}'
# -> 201 { customer, name_matches:[...] }; the name is not unique, so an existing
#    match is a warning, never a 409

curl http://localhost:3000/api/customers
# -> { customers: [{ customer, balance, over_limit }] }
curl http://localhost:3000/api/customers/1
curl -X PUT http://localhost:3000/api/customers/1 \
  -H "Content-Type: application/json" \
  -d '{"name":"Ana P.","credit_limit":null,"payment_days":15}'
# -> 200 { customer, balance, over_limit } (a null limit means no limit)
curl -X POST http://localhost:3000/api/customers/1/deactivate
curl -X POST http://localhost:3000/api/customers/1/activate
curl -X DELETE http://localhost:3000/api/customers/1
# -> 204 when unused; 400 when sales reference it or it is the walk-in (deactivate
#    instead)

curl "http://localhost:3000/api/customers/1/statement?as_of=2024-06-30"
# -> { customer, statement: { balance, as_of, ageing: { current,
#      overdue_1_30, overdue_31_60, overdue_61_plus }, entries: [chronological
#      ledger of sale debits and payment credits with the running balance] } }
curl "http://localhost:3000/api/customers/ageing?as_of=2024-06-30"
# -> { ageing: [{ customer_id, name, balance, over_limit, ageing }] }: every
#    customer with a non-zero balance

# Customer receipts: collect an amount, oldest debt first
curl -X POST http://localhost:3000/api/customer-receipts \
  -H "Content-Type: application/json" \
  -d '{"customer_id":1,"method_id":1,"amount":"60","date":"2024-06-20","notes":"partial"}'
# -> 201 receipt detail with the derived total and one allocation per covered
#    sale, each keeping its own transaction_id; > outstanding => 400, unassigned
#    method => 400, both with no side effect
curl "http://localhost:3000/api/customer-receipts?customer_id=1"
# -> { receipts: [receipt detail with allocations] }
curl http://localhost:3000/api/customer-receipts/1
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
  -d '{"method_id":1}'
# -> 200 detail with purchase_number "2024-PURCH-000001"; receives stock (In,
#    reason Purchase) and updates the satellite cost. Credit: send {} (no
#    method), due = total, no Expense. Unassigned method => 400
#    with no stock/finance touch. Double confirm => 400.

curl -X POST http://localhost:3000/api/purchases/1/payments \
  -H "Content-Type: application/json" \
  -d '{"method_id":1,"amount":"500","date":"2024-05-10"}'
# -> 201 payment + 1 Expense (Credit purchases; sum <= total; overpay => 400;
#    unassigned method => 400 with no finance touch)

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

`GET /login` — the login screen (S1b): the only page an anonymous visitor can reach. It renders a centered card without the app navigation — no sidebar, no logout button — with the username and password fields, a hidden `next` (a validated local path or empty) and the dismissible notice region; a failed attempt re-renders the page with the generic Spanish message (`Usuario o contraseña incorrectos`), which deliberately says nothing about whether the username exists. A successful login sets the session cookie and redirects to the validated `next` or `/`. Behind a session, `POST /logout` revokes it and clears the cookie (a plain form, no JavaScript needed).

Machine clients use the same surface over JSON: `POST /api/sessions` with `{"username":"…","password":"…"}` answers `204` plus the session cookie, and `DELETE /api/sessions` revokes and clears it (idempotent, `204`).

`GET /` — dashboard:

- Total balance (derived)
- Account list with balances (HTMX `GET /web/accounts`, OOB swap)
- Recent transactions filtered by account/date (`GET /web/transactions`)
- Forms:
  - Create account: `POST /web/accounts` (HTMX, `hx-post`, no reload)
  - Create transaction: `POST /web/transactions` (HTMX)

`GET /accounts/:id` — detail with history + delete buttons (`hx-delete`).

`GET /products` — inventory:

- Product list with derived stock + low/negative badges (HTMX `GET /web/products`)
- Filter bar (`#product-filters`): text search over name, SKU and barcode (`q`) plus
  `category_id`; the filter is addressable (`/products?q=…&category_id=…`) and Back restores
  the list it was applied to
- Every product mutation that answers a list renders the list the caller is looking at, never
  the whole catalogue: those forms carry `hx-include="#product-filters"`, so the handler reads `q`
  and `category_id` straight from the body. `POST /web/products/edit` is the exception — its
  non-drawer branch reads the filter from the query string, because its body already owns
  `category_id`; no page reaches that branch, since the drawer Save answers the detail fragment.
  The New product modal's own category select is `product_category_id`, because the filter
  already owns `category_id` and htmx resolves a name collision in favour of the form's own field
- Creating a product that the active filter hides answers with a notice naming the product,
  stating that the filter is keeping it out of the list, and offering `Clear filter`; the
  filter itself is left alone
- Low-stock list with reorder suggestion (HTMX `GET /web/low-stock`, also `GET /web/negative-stock`)
- Forms:
  - Create category: `POST /web/categories` (HTMX)
  - Create product: `POST /web/products` (HTMX)
  - Record movement: `POST /web/stock-movements` (HTMX)

`GET /sales` — sales:

- Sale list with status/debt badges; each row links to its record (HTMX `GET /web/sales`)
- `/sales/:id` — record page with the header (status, number or draft state, customer,
  dates, totals, payment status), the lines table with product name and SKU, and the
  payments table with account and method names; an unknown id is a 404
- Outstanding debt — Confirmed sales with due > 0 (HTMX `GET /web/sales/debt`)
- Actions, gated by status and offered in context on the record page:
  - New sale Draft: `POST /web/sales` (HTMX, answers `HX-Redirect` to the new record)
  - Add line (Draft): `POST /web/sales/:id/lines` (HTMX)
  - Edit header (Draft): `POST /web/sales/:id/header` (HTMX)
  - Confirm: `POST /web/sales/:id/confirm` (HTMX)
  - Record payment (Confirmed, Credit): `POST /web/sales/:id/payments` (HTMX)
  - Cancel (Confirmed) / discard (Draft): `POST /web/sales/:id/cancel` (HTMX,
    `hx-confirm` asks first)

`GET /customers` — customers (M4):

- Names-only customer list; an inactive name renders muted (HTMX `GET /web/customers`)
- New customer button opens a `<dialog>` modal with the create form
  (`POST /web/customers`, HTMX; a duplicate name renders the existing matches
  as a warning)
- Selecting a name opens the right slide-over drawer with the customer detail
  (HTMX `GET /web/customers/detail/:id`); no edit card is shown (the
  `POST /web/customers/edit` endpoint stays available)
- Forms (id in the body):
  - Edit customer: `POST /web/customers/edit` (HTMX; the fields replace the
    current values and empty optional fields clear)
  - Activate/deactivate: `POST /web/customers/activate|deactivate` (HTMX)
  - Delete: `POST /web/customers/delete` (HTMX, RESTRICT-aware)

`GET /customers/:id` — customer statement (same list, drawer open):

- Ageing breakdown, receivable sales and the chronological ledger (HTMX
  `GET /web/customers/detail/:id`)
- Payment history: every receipt with its allocations (HTMX
  `GET /web/customers/:id/receipts`)
- Collect form: the customer, the amount, the account and the method; applies the
  amount oldest debt first and creates one receipt grouping one payment per
  covered sale (`POST /web/customer-receipts`, id in the body)

`GET /purchases` — purchases (M3):

- One row per purchase: identifier, supplier, date and item count, a neutral
  total, and a single status chip carrying the residual amount when something
  is owed (Draft · Paid · Due · Overdue · Cancelled); each row opens its
  read-only peek (HTMX `GET /web/documents/detail/purchase/{id}`) and links
  to its record (HTMX `GET /web/purchases`)
- `/purchases/:id` — record page with the header (status, number or draft state,
  supplier, dates, totals, payment status), the lines table with product name and
  SKU, unit cost and subtotal, and the payments table with account and method
  names; the product picker matches name, SKU and barcode and shows current stock;
  an unknown id is a 404, and so is a malformed one (`/purchases/abc`
  answers 404, not the path extractor's 400 — the route takes `Path<String>`
  and parses the id itself, which is also what let `/purchases/new` be
  deleted); note the deliberate inconsistency with the `/web/purchases/{id}`
  fragments, which keep `Path<i64>` and still answer 400 for a malformed id
- creation dialog (T3): the `/purchases` page's "New purchase" header action
  opens `#new-purchase-dialog` for a `purchases.create` principal (a read-only
  one renders no action at all); the dialog holds the supplier picker
  pre-filled with the last used supplier, and choosing posts the existing
  `POST /web/purchases`, which answers `HX-Redirect` to the new record
  (`/purchases/{id}`); a typed name resolves server-side, an unknown name
  refuses, and an absent date defaults to today. `/purchases/new` is deleted
- Sugerido panel rendering the suggestion with a `→ Draft` seed button per row
  (HTMX `GET /web/purchases/suggestions`, seed via `POST /web/purchases/from-suggestion`,
  which opens the new draft's record)
- Actions, gated by status and offered in context on the record page:
  - Add line (Draft): `POST /web/purchases/:id/lines` (HTMX, scanner or picker; the
    repeated-product rule is a clear 400)
  - Draft header (in place): on a draft the record page's header is the edit
    form itself — supplier (through the T2 picker, same contract as the
    creation dialog), purchase date, supplier invoice no and notes — posting to
    `POST /web/purchases/:id/header` (HTMX); on a confirmed or cancelled
    purchase the header stays read-only text. The Edit header dialog and its
    `⋯` menu entry are gone. Since the header can now change the supplier, a
    purchase's supplier is no longer fixed at creation — on a draft it
    resolves exactly like the creation flow (explicit picker id wins, else the
    typed name must resolve, or the route refuses)
  - Confirm: `POST /web/purchases/:id/confirm` (HTMX)
  - Record payment (Confirmed, Credit): `POST /web/purchases/:id/payments` (HTMX)
  - Cancel (Confirmed) / discard (Draft): `POST /web/purchases/:id/cancel` (HTMX,
    `hx-confirm` asks first)

`GET /suppliers` — suppliers + cost satellite (M3):

- Names-only supplier list; an inactive name renders muted (HTMX `GET /web/suppliers`)
- New supplier button opens a `<dialog>` modal with the create form
  (`POST /web/suppliers`, HTMX); no edit card is shown (the
  `POST /web/suppliers/edit` endpoint stays available)
- Selecting a name opens the right slide-over drawer with the supplier header,
  the outstanding balance (sum of `due` over Confirmed purchases) and that
  supplier's purchases linking to their records (HTMX
  `GET /web/suppliers/:id/detail`)
- Forms:
  - Create supplier: `POST /web/suppliers` (HTMX)
  - Edit supplier: `POST /web/suppliers/edit` (HTMX)
  - Activate/deactivate: `POST /web/suppliers/:id/activate|deactivate` (HTMX)
  - Delete supplier: `DELETE /web/suppliers/:id` (HTMX, RESTRICT-aware)
  - Record product cost: `POST /web/supplier-costs` (HTMX)

`GET /documents` — documents index (cross-department):

- One feed over six document families, newest first: sales, sale payments, purchases, purchase
  payments, stock movements and customer receipts (HTMX `GET /web/documents`)
- The page opens for ANY ONE of `sales.read`, `purchases.read`, `inventory.read` and
  `customers.read`, and narrows its content per tier: a `sales.read`-only principal sees the sale
  documents and their payments, never the purchases, the stock movements or the receipts — not even
  the type option. Movements of cash (`transactions`) are out of scope by decision
- Filter bar (`#document-filters`): type (`group`; the operator's four options — Ventas, Compras,
  Movimientos de stock, Pagos — and only the permitted ones render), acting user (`user`,
  a display-name or username substring), inclusive date range (`from`/`to`) and the text search
  (`q`, matched against the document number/reference and the counterpart's name). The filter is
  addressable (`/documents?group=…&q=…`) and Back restores the list it was applied to
- Every row carries the document's date, family, identifier, counterpart, status/detail, its amount
  or stock quantity, and the acting user's display name; clicking the identifier opens a side drawer
  (`GET /web/documents/detail/{kind}/{id}`) with the document's full information, narrowed per
  family the same way the rows are (a principal cannot open another tier's document — the refusal
  names the read code it lacks); `Open` links to the page that owns the document (`/sales/{id}`,
  `/purchases/{id}`, `/customers/{id}`), and a stock movement opens the
  products list at that product's row, because there is no product record page
- Drawer actions (real ones only, never invented, each gated by the code its endpoint requires):
  - **Draft sale/purchase** — "Eliminar borrador" (`DELETE /web/sales/{id}` / `DELETE
    /web/purchases/{id}`, `sales.create` / `purchases.create`) with a confirm; a draft never left
    stock, money or ledger entries, so this is the only document delete in the system. "Descartar"
    (`POST /web/sales/cancel` / `POST /web/purchases/cancel`, `sales.cancel` / `purchases.cancel`)
    turns a draft into Cancelled with nothing to revert
  - **Confirmed sale/purchase** — "Anular" over the existing cancel endpoint (optional reason): the
    designed inverse that writes one return movement per tracked line and one refund per payment,
    and stops the debt
  - **Impact preview before every press** — the drawer lists, computed server-side from the same
    reads the endpoint will use, exactly what will be deleted (the draft and its listed lines) or
    created (each `In · Sale-return` / `Out · Purchase-return` movement, each `Expense`/`Income`
    refund with amount and account, the debt effect), plus a plain refusal warning when a tracked
    product is now inactive or a refund could push an account negative (`allow_negative = false`)
  - **Payments, receipts and stock movements get no button** — the drawer explains why instead: the
    money is already in the ledger, the receipt groups payments the database keeps while it
    explains them, and stock history is append-only (compensate with an adjustment on the product)
  - After an action the endpoint answers `HX-Trigger` (`sale-changed` / `purchase-changed`); the
    page closes the drawer and re-reads the feed
- The feed shows the newest 200 documents and states when the cap cut the history instead of
  pretending the history ended
- The index itself still writes nothing of its own: the drawer only re-presents each document's own
  existing actions (draft delete, annul/discard), each gated by the permission its endpoint already
  requires

All forms use HTMX; server returns HTML fragments (`partials/*`) and `HX-Trigger` events for refresh. HTMX 1.9.12 is served locally from `/static/htmx.min.js` (no CDN).

Navigation: the sidebar groups destinations into Operation (Dashboard, Sales, Purchases, Documents), Catalogue (Products, Suppliers, Customers), Cash (Accounts, currently the dashboard section) and Account (Users, Roles, Password). Each page's rendering struct carries a nav view built from the request's principal, so the server renders only the entries that principal may read (each entry declares the permissions its href and named blocks need — the mapping lives in `authz::NAV_ENTRIES` and is drift-tested) and hides a group heading when nothing in it is visible; the active entry is marked server-side, so the state is correct without JavaScript. The signed-in user's display name and username sit next to the logout control. The environment line (`local · SQLite`) and the REST API link sit below the groups. Failed and successful actions report through the dismissible `#notice` region instead of a blocking browser dialog; forms name the action with `data-action` and fall back to the request path. A response that carries its own server-rendered notice wins over the generic `<action> saved` text: the create-under-filter answer swaps its notice out of band into `#notice` (`templates/partials/notice.html`) and marks it `data-notice-server`, which `base.html` reads to skip the generic one. The name travels in the body rather than an `HX-Trigger` payload because product names are arbitrary UTF-8: a raw non-ASCII header value reaches the client as mojibake, since XHR decodes header bytes as ISO-8859-1 (`HeaderValue` itself accepts bytes >= 0x80), and re-encoding the name into ASCII by hand is the fragile part, not the header.

## Styles & local assets

The UI is styled with **Tailwind CSS 4.3.3** utilities (Tailwind v4 syntax: CSS-first configuration, no `tailwind.config.js`). The source of truth is `assets/tailwind.css`; the compiled stylesheet is committed at `static/tailwind.css`. Both files under `static/` are served by Axum at `/static/*` (`tower-http` `ServeDir` nested in `routes::router`), so the app has no CDN dependencies:

- `static/tailwind.css` — compiled Tailwind 4.3.3 stylesheet (~13 KB, only the classes in use)
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
| `ENFORCE_CREDIT_LIMIT` | `true` | If `true`, confirming a credit sale whose projected debt exceeds the customer's `credit_limit` is rejected (400); a null limit is unlimited either way. If `false`, the sale is confirmed and the interface reports the customer as over limit |
| `PORT` | `3000` | HTTP port |
| `RUST_LOG` | `info` | tracing filter |
| `ROYA_ADMIN_PASSWORD` | *(unset)* | The bootstrap administrator's password. Unset means a password is generated and logged once at startup (with `must_change_password` set), so the operator must read the log to get in; set, the value is used as-is and never logged |
| `ROYA_SESSION_TTL_HOURS` | `12` | Absolute session lifetime in hours: the cookie's `Max-Age` and the session row's expiry are set together |
| `ROYA_COOKIE_SECURE` | `false` | **Required when the app is served over HTTPS**: set it to `true` or `1`, or the session cookie is not marked `Secure` and can travel over plain HTTP. Without HTTPS, leave it off — browsers drop `Secure` cookies on plain HTTP, which would make login impossible |
| `ROYA_ALLOWED_ORIGINS` | *(unset)* | Comma-separated list of origins (`https://app.example.com`) allowed by CORS. Unset = same-origin only: no wildcard, and no `Access-Control-Allow-Origin` header is ever emitted (the old wildcard is gone) |
| `ROYA_LOGIN_THROTTLE_ATTEMPTS` | `5` | Failed logins per username before the throttle kicks in |
| `ROYA_LOGIN_THROTTLE_SECONDS` | `60` | Throttle window in seconds: further attempts on a throttled username return the generic error until it elapses |

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
- `Sale` rules: Draft editable (lines/dates/notes); Confirmed/Cancelled immutable
  except Cancel. The customer is fixed at creation: `customer_id` is mandatory
  (`NOT NULL`, RESTRICT) and `customer_name` is the customer's name snapshotted
  at creation, never rewritten by later corrections. Credit to the walk-in
  returns 400; with `ENFORCE_CREDIT_LIMIT=true` and a customer limit, confirming
  a credit sale whose projected debt (`confirmed credit sales − their payments`
  plus this sale) exceeds the limit returns 400 with the projected figure; a
  null limit is never checked and `ENFORCE_CREDIT_LIMIT=false` confirms the sale
  (the customer reads as over limit). Credit without `due_date` defaults to
  `sale_date + payment_days`, and without a term the due date is required.
  `sale_number` immutable once set (`YYYY-SALE-NNNNNN`), NULL only in
  Draft/Cancelled-from-Draft. Cash forbids `due_date`. `qty > 0`,
  `unit_price >= 0`, payments name only the `method_id` (N per sale,
  mixed methods, sum ≤ total), reject overpay (`paid + amount <= total`) and
  unassigned/inactive methods (400, no stock/sequence/finance touch).
- `PaymentMethod` rules: `account_id` NULL (unassigned) or one owning account,
  `UNIQUE(account_id, name)`, `is_active` 0/1; `sale_payments.method_id` RESTRICT
  NOT NULL; unknown method => 404, inactive/unassigned => 400 with an actionable
  message.
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
  it. `qty > 0`, `unit_cost >= 0`, no duplicated product per purchase, payments name
  only the `method_id` (mixed methods, sum ≤ total), reject overpay and
  unassigned/inactive methods (400, no stock/sequence/finance touch). Confirm updates the
  satellite per line; cancelling a Confirmed purchase returns stock and refunds paid
  amounts as Income. No new env vars for purchases (reuses `ALLOW_NEGATIVE_BALANCE` /
  `ALLOW_NEGATIVE_STOCK`).
- `Customer` rules: `name` trimmed, non-empty, ≤128 and **not unique**; `phone`/`tax_id`
  ≤32, `address` ≤256, `notes` ≤512 (empty clears to NULL); `credit_limit ≥ 0` NULL =
  no limit, `payment_days ≥ 0` NULL = no default term. The seeded walk-in cannot be
  deleted or deactivated (service check plus database triggers), cannot be demoted and
  cannot be created twice; deleting any customer with sales is refused (RESTRICT) while
  deactivation keeps the history.
- `CustomerReceipt` rules: the receipt is a grouping document with no stored total. Its
  derived amount is `SUM(sale_payments.amount WHERE receipt_id = receipt)`. Collecting
  validates the customer (404), a positive amount, the method's owning account
  (400 when unassigned/inactive) and `amount ≤ customer balance` (400) before any write, then
  applies the amount oldest-first. A payment may only be grouped under a receipt of its
  own customer (service guard + database trigger), and deleting a referenced receipt is
  refused (RESTRICT). A payment without a receipt is still a direct payment on one sale.

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
                           — also exposes the derived customer receivable (balance, statement, ageing)
src/services/customers.rs  — customer CRUD, walk-in protection, duplicate-name warning
src/services/customer_receipts.rs — collect oldest-first: one receipt grouping one payment per covered sale
src/services/suppliers.rs  — supplier CRUD + product/supplier satellite cost rule
src/services/purchases.rs  — Draft/Confirm/Pay/Cancel + suggestion builder (orchestrates stock, finance, satellite)
src/services/documents.rs   — the cross-department documents index: composes the four families'
                             reads, merges newest-first, caps the page at 200 and reports when it cut
src/services/identity.rs   — identity: bootstrap admin, login with constant-time verification and
                             the generic failure, the in-memory throttle, session mint/resolve/
                             renew/revoke, password change, the users and roles tier rules
src/repositories/account_repo.rs
src/repositories/transaction_repo.rs
src/repositories/category_repo.rs
src/repositories/product_repo.rs
src/repositories/barcode_repo.rs
src/repositories/stock_repo.rs
src/repositories/sale_repo.rs          — Sale/SaleLine/SalePayment SQLite impl
src/repositories/customer_repo.rs      — Customer SQLite impl (RESTRICT-aware delete)
src/repositories/customer_receipt_repo.rs — receipt document + allocations read
src/repositories/supplier_repo.rs      — Supplier SQLite impl (RESTRICT-aware delete)
src/repositories/product_supplier_cost_repo.rs — satellite cost SQLite impl
src/repositories/purchase_repo.rs      — Purchase/PurchaseLine/PurchasePayment SQLite impl
src/repositories/user_repo.rs          — User SQLite impl (NOCASE username, active flag)
src/repositories/session_repo.rs       — Session SQLite impl (SQL-decided validity, sliding renewal)
src/repositories/role_repo.rs          — Role SQLite impl (holders, grant replacement)
src/repositories/permission_repo.rs    — Permission SQLite impl (catalog, role matrix)
src/repositories/doc_sequence_repo.rs  — atomic YYYY-SALE-NNNNNN / YYYY-PURCH-NNNNNN numbering
src/routes/api.rs
src/routes/web.rs
src/routes/inventory_api.rs
src/routes/inventory_web.rs
src/routes/sales_api.rs    — REST /api/sales, lines, payments, confirm/cancel, debt
src/routes/sales_web.rs    — Web /sales Askama + HTMX
src/routes/customers_api.rs — REST /api/customers, statement, ageing, /api/customer-receipts
src/routes/customers_web.rs — Web /customers + statement Askama + HTMX
src/routes/purchases_api.rs — REST /api/suppliers, /api/product-supplier-costs, /api/purchases
src/routes/purchases_web.rs — Web /purchases Askama + HTMX (incl. Sugerido)
src/routes/suppliers_web.rs — Web /suppliers Askama + HTMX
src/routes/identity_web.rs — GET/POST /login, POST /logout, GET/POST /password
src/routes/identity_api.rs — POST/DELETE /api/sessions (JSON session API)
src/routes/users_web.rs    — Web /users: list, create, deactivate/activate, password reset, roles
src/routes/roles_web.rs    — Web /roles: list, create, edit, delete, permission matrix
src/routes/documents_web.rs — Web /documents: the cross-department index page + its list fragment (any-of read gate)
src/routes/mod.rs
templates/base.html
templates/dashboard.html
templates/account_detail.html
templates/login.html
templates/password.html
templates/forbidden.html   — the full-page 403 card (navigation obeys the same rule)
templates/users.html
templates/roles.html
templates/products.html
templates/sales.html
templates/customers.html
templates/purchases.html
templates/suppliers.html
templates/documents.html
templates/partials/*.html  — incl. sale_list.html, sale_detail.html, purchase_list.html,
                             purchase_detail.html, supplier_list.html, suggestion_list.html,
                             customer_list.html, customer_statement.html, receipt_list.html,
                             sidebar.html (the permission-gated navigation), user_list.html,
                             role_list.html, user_roles_form.html, user_password_form.html,
                             document_list.html
migrations/*.sql
assets/tailwind.css        — Tailwind v4 entrypoint (@source templates/, @theme palette)
static/tailwind.css        — compiled stylesheet (committed; rebuild via scripts/build-css.sh)
static/htmx.min.js         — HTMX 1.9.12 served locally (no CDN)
scripts/build-css.sh       — regenerates static/tailwind.css with the standalone CLI
```

`src/templates/` placeholder exists for spec compliance; Askama loads from `templates/` at crate root (standard).

## No Heavy ORM / No Docker

- No Diesel/SeaORM.
- No microservices, no mandatory Docker (just `cargo run`).
- **Auth (S1/S2 identity)**: every route except `/login` (GET/POST), `POST/DELETE /api/sessions`,
  `/static/*` and `GET /favicon.ico` is behind a deny-by-default session gate — an undeclared route is still refused,
  and every department handler additionally declares the permission its action needs
  (`Require<P>`): refused with `403` in the shape the caller reads (JSON for `/api/*` and HTMX, an
  HTML card for a full page). The bootstrap `admin` user is seeded at startup (password from
  `ROYA_ADMIN_PASSWORD` or generated and logged once, confined to `/password` until changed); the
  session cookie is `HttpOnly`, `SameSite=Lax`, `Path=/`, with a 12h absolute TTL, a 30-minute
  sliding renewal and revocation on logout; failed logins are throttled per username and unsafe
  HTTP methods are refused cross-origin. Roles and their permission matrices are editable at
  `/roles`, users at `/users` (tiered: reads `identity.users.read`, mutations
  `identity.users.manage`, role sets and the roles screen `identity.roles.manage`), and the sidebar
  renders only the entries the principal may read.
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
  `refund_transaction_id`). The invariant also rejects orphan document
  movements: any transaction whose `reference` looks like `YYYY-SALE-NNNNNN`
  or `YYYY-PURCH-NNNNNN` must be claimed by some payment as `transaction_id`
  or `refund_transaction_id`, so a failure between creating the movement and
  inserting the payment row cannot hide, and the offending ids are reported.
- **Customer receipts (Slice L)**: a receipt's amount is derived from the
  payments it groups, and database triggers refuse to group a payment under a
  receipt of another customer on insert and on update alike, so no code path can
  make a receipt claim money its own collection never applied. A service-level
  refusal maps that trigger abort to a clean 400, and no route accepts a receipt
  id: the collect request carries only the customer, the amount, the account and
  the method. Ungrouped payments and same-customer groupings are unaffected.
- **Customers collection flow (Slice M)**: the suite creates a customer through
  the web form, sells 3 × 25 on credit, collects 30 through
  `POST /web/customer-receipts`, then asserts the derived balance (45), the
  ageing bucket (`overdue_1_30 = 45` against `as_of=2024-06-20`), the
  receivables view, the receipt's derived total equal to the sum of its
  allocations, each grouped payment's `transaction_id`, and the
  money-traceability invariant over the whole database the flow built.
- **Generic form-wiring guard**: for the seeded `/`, `/accounts/{id}`,
  `/products`, `/sales`, `/purchases`, `/suppliers` and `/customers` pages (plus
  the sale and purchase detail fragments and the `/customers/{id}` statement) it extracts every `hx-get`, `hx-post`, `hx-put`,
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
- **Typed-id shells**: `/`, `/sales`, `/purchases` and `/customers` are the
  pages where the user types the id into the form, so a concrete numeric path
  segment in a form-bound target is rejected (`/web/sales/1/confirm`) while
  data-bound record links such as the list View buttons stay valid. A `:` is a
  placeholder marker only in the path, so `datetime` query values pass.
- **Native forms**: a `this.action=` rewrite inside `onsubmit` fails the guard
  (htmx ignores the form action property), and native `action=` targets are
  probed with their form method like any other target.
- **Referenced-id guard**: every seeded page is scanned for `product #`,
  `account #`, `method #`, `customer #` and `supplier #` followed by digits, so a
  list or fragment can never leak the internal id of a referenced entity. A
  document's own id stays allowed (`draft #12`, `2024-SALE-000012`), and the scan
  is pinned by a page-copy mutation test. The receipt list resolves account and
  method names through the finance read paths, and the guard fixture collects a
  receipt so the customer statement renders that list and the rule covers it.

## Tests (browser suite)

A second toolchain lives in `e2e/`: a Playwright browser suite that drives the
real interface, because interaction, focus, navigation and dialogs are only
honest in a browser. One command runs it, it never touches `roya.db`, and it
needs nothing installed or managed for Node (Playwright's Python driver bundles
its own Node runtime inside the suite's virtual environment):

```bash
scripts/e2e.sh          # headless; see e2e/README.md for setup and options
```

The suite currently covers 68 tests (plus 4 opt-in probes — two screenshot probes and two
artifact probes — that skip by default, `ROYA_E2E_*_PROBE=1`). It builds the binary once, spawns it
against a throwaway SQLite file on a free port whose isolation it proves from the server's own
log, seeds data through
the HTTP API reading each step's effect back, and writes a Playwright trace,
screenshot and server log only when a test fails. Because the login gate is
deny-by-default, the harness spawns the binary with a fixed test
`ROYA_ADMIN_PASSWORD` and logs in once per server: the HTTP-API seeder and every
browser context share that session cookie, so no test handles login itself
(the gate's own behaviour is covered in `e2e/tests/test_identity.py`).
`cargo test` stays independent
of it and keeps its current speed; business rules remain in the Rust suite. The
one-time Chromium download, the covered flows and the deliberate boundary are
documented in [`e2e/README.md`](e2e/README.md).

## License

MIT
