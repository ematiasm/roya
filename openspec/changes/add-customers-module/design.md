# Design: add-customers-module

## Architecture
```
routes/customers_api.rs + customers_web.rs -> CustomerService
  -> CustomerRepository + CustomerReceiptRepository
  -> SalesService (read sales and their payments for the balance, apply a receipt through it)
  -> PaymentMethodService (allowlist check, reused from finance)
  -> TransactionService (the movement of each allocation, reached through SalesService)
```
Customers sit **above** sales, exactly like sales sit above finance and inventory: customers may read
sales, sales never read customers' tables, and finance never learns that customers exist. Balance and
ageing are derived by reading confirmed credit sales and their payments, never stored.

## Migrations
1. `create_customers` — table, index on `is_active`, index on `is_walkin`, and the seeded walk-in row
   inserted with a guarded `INSERT ... WHERE NOT EXISTS` so re-running cannot duplicate it.
2. `add_sales_customer` — `ALTER TABLE sales ADD COLUMN customer_id INTEGER REFERENCES customers(id)`,
   then backfill every existing row to the walk-in, then the table rebuild that enforces `NOT NULL`
   (SQLite cannot add a NOT NULL column with a foreign key directly), preserving rows.
3. `create_customer_receipts` — table, FKs RESTRICT, indexes.
4. `add_sale_payments_receipt` — `receipt_id INTEGER NULL REFERENCES customer_receipts(id)`, indexed,
   plus the unique-per-allocation invariant left to the service.

## Key decisions and tradeoffs
| Option | Chosen | Why / cost |
|---|---|---|
| Walk-in customer vs nullable `customer_id` | Seeded walk-in, `NOT NULL` | Every sale has an owner, no null cases in the model, and `Consumidor Final` is required by real invoicing later; cost: the walk-in accumulates sales and must be excluded from credit rules by a flag |
| One receipt-and-allocation model vs per-sale payments | Per-sale payments, with a receipt grouping them | Keeps the existing tested payment path, traceability per payment and cancellation logic intact; cost: a lump-sum handover produces several finance rows, and the receipt invariant must be enforced rather than implied |
| One movement per receipt (true accounting model) | Deferred | Would move the finance link from the payment to the receipt and rewrite the cancellation path; the allocation data already has the right shape, so the migration stays open and cheap |
| Credit limit as a hard rule | Configurable flag, default enforcing | A limit that does not limit is pointless, but a shop may need to override one sale without deleting the limit; cost: two states to reason about, so a null limit is documented as the way to say "no limit" |
| Unique customer name | Not unique, duplicates reported | Two people share names; blocking forces invented names; cost: the interface must warn and the user must disambiguate |
| Opening balances and adjustments | Out of scope | The user confirmed no pre-existing debt needs migrating; cost: a debt older than the system cannot be recorded without a fake sale, to be revisited if it comes up |
| Ageing stored vs derived | Derived from `due_date` | Consistent with balance, stock and debt; cost: it is computed per read |
| Where the receipt lives | A customer document, not a sale document | One handover can cover several sales, so it cannot belong to one of them |

## Impact on existing code
- `sales.customer_id` becomes mandatory, so these need updating: `SalesService::create` and its DTOs,
  the sales REST and web handlers, the sale creation form, the route tests and the smoke suite (which
  create sales without a customer).
- The existing sale in the database is backfilled to the walk-in; its `customer_name` snapshot is left
  untouched.
- No change to finance or inventory beyond reading through their services.

## HTMX parity
- `/customers` list with balance, over-limit and ageing badges; `/customers/{id}` statement with the
  ageing breakdown, the receivable sales and the payment history; a collect form that takes an amount
  and applies it oldest-first, creating one receipt with one payment per covered sale.
- Every form follows the wiring rules the guard enforces: collection endpoints with the id in the body,
  no hardcoded ids, no dynamically built URLs.

## Verification plan
Service-level tests for AC1–AC9 and AC14–AC16; receipt tests for AC10–AC13; a smoke test that runs a
credit sale, collects part of it, checks the derived balance and ageing, and asserts the receipt total
equals the sum of its payments; and the money invariant extended so a receipt-linked payment still
carries a `transaction_id` whose reference matches the sale number.
