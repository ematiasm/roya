# Roya — Local Personal Finance Tracker

Local CRUD for personal finances: 2–3 accounts/wallets, income/expense transactions, derived balances, REST API + HTMX web UI. 100% local, ready to scale.

Stack: **Rust + Axum 0.8.9 + Tokio + SQLx 0.9 (SQLite → Postgres) + Askama + HTMX + rust_decimal + chrono**.

## Features

- **Account** — `id, name, cached_balance, created_at`
- **Transaction** — `id, account_id (FK), kind (Income/Expense), amount (Decimal), description, date (NaiveDate), created_at`
- Balance is **always derived** `SUM(Income) - SUM(Expense)` — `cached_balance` is kept in sync transactionally but never trusted for reads.
- Validation: `amount > 0`, negative balance blocked when `ALLOW_NEGATIVE_BALANCE=false`.
- Dashboard with total balance across all accounts.
- **REST API** and **Web (Askama + HTMX, no page reload)**.
- **Sales** — Draft → Confirmed → Cancelled orchestrator (Odoo-style):
  - Draft creates lines with no stock/finance side effects.
  - Confirm assigns `YYYY-SALE-NNNNNN`, deducts stock (`Out`, reason `Sale`),
    Cash posts 1 Income, Credit opens a receivable (due = total).
  - Credit payments each post 1 Income; overpay ⇒ 400; Paid when due = 0.
  - Cancel of Confirmed re-enters stock (`In`, reason `Sale-return`) and posts
    Expense refunds, guarded by `ALLOW_NEGATIVE_BALANCE`.
  - `sale_number` UNIQUE, immutable, NULL only in Draft/Cancelled-from-Draft.
  - Finance/stock rows are written only via services, reference = `sale_number`.

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

# Transactions
curl "http://localhost:3000/api/transactions?account_id=1&from=2024-01-01&to=2024-12-31"

curl -X POST http://localhost:3000/api/transactions \
  -H "Content-Type: application/json" \
  -d '{"account_id":1,"type":"Income","amount":"1000.50","description":"Salary","date":"2024-01-15"}'

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
  -d '{"account_id":1}'
# -> 200 detail with sale_number "2024-SALE-000001"; deducts stock,
#    Cash posts 1 Income. Credit: send {} (no account), posts nothing, due = total.
#    Double confirm => 400.

curl -X POST http://localhost:3000/api/sales/1/payments \
  -H "Content-Type: application/json" \
  -d '{"account_id":1,"amount":"15","date":"2024-05-10"}'
# -> 201 payment + 1 Income (Credit sales; overpay => 400)

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

Error format:

```json
{ "error": "amount must be > 0" }
```

- `400` validation (empty name, amount <=0, from>to, insufficient funds, sale guards below)
- `404` not found (incl. unknown product/account on sale confirm/pay)
- `409` duplicate account name (UNIQUE), duplicate sku/barcode/receipt_no/sale_number

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

All forms use HTMX; server returns HTML fragments (`partials/*`) and `HX-Trigger` events for refresh. HTMX loaded via CDN `https://unpkg.com/htmx.org@1.9.12`.

## Configuration

| Env | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | `sqlite://roya.db` | SQLite file (or `postgres://...`) |
| `ALLOW_NEGATIVE_BALANCE` | `false` | If `false`, Expense that would make balance negative is rejected (also on edit/delete of Income) |
| `ALLOW_NEGATIVE_STOCK` | `true` | If `false`, Out that would make stock negative is rejected (400, stock unchanged); if `true`, Out succeeds (201) and product appears in negative list |
| `PORT` | `3000` | HTTP port |
| `RUST_LOG` | `info` | tracing filter |

## Validation & Balance Rules

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
  `qty > 0`, `unit_price >= 0`, payments reject overpay (`paid + amount <= total`).
  No new env vars for sales (reuses `ALLOW_NEGATIVE_BALANCE` / `ALLOW_NEGATIVE_STOCK`).
  - Balance read path: `SELECT kind, amount FROM transactions WHERE account_id=?` summed in Rust (not `SUM()` which would cast TEXT→REAL).

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
src/repositories/account_repo.rs
src/repositories/transaction_repo.rs
src/repositories/category_repo.rs
src/repositories/product_repo.rs
src/repositories/barcode_repo.rs
src/repositories/stock_repo.rs
src/repositories/sale_repo.rs          — Sale/SaleLine/SalePayment SQLite impl
src/repositories/doc_sequence_repo.rs  — atomic YYYY-SALE-NNNNNN numbering
src/routes/api.rs
src/routes/web.rs
src/routes/inventory_api.rs
src/routes/inventory_web.rs
src/routes/sales_api.rs    — REST /api/sales, lines, payments, confirm/cancel, debt
src/routes/sales_web.rs    — Web /sales Askama + HTMX
src/routes/mod.rs
templates/base.html
templates/dashboard.html
templates/account_detail.html
templates/products.html
templates/sales.html
templates/partials/*.html  — incl. sale_list.html, sale_detail.html
migrations/*.sql
```

`src/templates/` placeholder exists for spec compliance; Askama loads from `templates/` at crate root (standard).

## No Heavy ORM / No Docker / No Auth

- No Diesel/SeaORM.
- No microservices, no mandatory Docker (just `cargo run`).
- No auth (local single-user).
- `tower-http` trace + cors only.

## Tests (manual)

```bash
# Run server then in another shell:
curl -X POST http://localhost:3000/api/accounts -H "Content-Type: application/json" -d '{"name":"Cash"}'
curl -X POST http://localhost:3000/api/transactions -H "Content-Type: application/json" -d '{"account_id":1,"type":"Income","amount":"500","description":"test","date":"2024-01-01"}'
curl "http://localhost:3000/api/transactions?account_id=1"
```

## License

MIT
