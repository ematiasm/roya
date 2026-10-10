# Payment method → single account (1:N)

## Objective
Cada método de pago pertenece a una sola cuenta (una cuenta tiene muchos
métodos). Todos los formularios de cobro/pago pasan a un solo select de
método (`"Transfer — Bank"`), derivando la cuenta en backend. Más rápido al
cobrar, cero combinaciones inválidas.

## Problem
Hoy el par `(account_id, method_id)` es muchos-a-muchos vía allowlist
(`account_payment_methods`): la UI exige elegir ambos y el backend rechaza
con 400 las combinaciones fuera de la lista. Elegir mal es un error
innecesario que la UI no debería permitir.

## Why
Velocidad al cobrar + imposibilidad de combinaciones inválidas por
construcción. Decisión explícita del usuario (scope: completa).

## Scope
- Nueva migración: `payment_methods.account_id`, UNIQUE(account_id,name),
  drop de `account_payment_methods`
- `src/repositories/payment_method_repo.rs`, `src/services/finance_methods.rs`
- Firmas que reciben el par: `customer_receipts.collect`,
  confirms/payments de sales y purchases
- REST: `PUT /api/accounts/:id/payment-methods`, creación de cuentas,
  `POST /api/customer-receipts`, confirms (breaking: documentar)
- Web: `account_detail.html`, dashboard, `sale_detail.html`,
  `purchase_detail.html`, `customer_detail.html` (solo-método)
- Seeds/defaults (`ensure_defaults_for_account` con duplicación)
- Tests: finance_methods, sales, purchases, web, e2e helpers, smoke
- Docs vivas: README, `openspec/specs/finance/spec.md` (y sales si aplica)
- Fuera de alcance: historial (los `method_id` guardados no se reescriben),
  commitear (decisión del usuario)

## Constraints
- SQLite: rebuild de tabla para cambiar constraints; migración con
  `INSERT OR IGNORE` donde aplique, idempotente en lo posible
- Métodos compartidos (gral: Transfer en Banco+MP) → duplicar fila por
  cuenta; huérfanos (QR sin cuenta) → `account_id NULL`, no utilizables
  hasta asignarse
- Recibos/pagos guardan su propio `account_id`: el historial queda intacto
- Base real del usuario verificada limpia (ningún método compartido hoy)

## Authorized scope
Migración completa + todos los formularios + API + tests + docs. Rama
`feat/payment-method-single-account`. NOTA: el árbol trae cambios sin
commitear del rediseño customers/suppliers (rama anterior); viajan en el
worktree, commitear por separado.

## Acceptance criteria
- [x] Un método pertenece a ≤1 cuenta a nivel DB (FK + UNIQUE)
      Closed 2026-10-02 **against the tree**: `migrations/20240101000024_payment_methods_single_account.sql:26` (FK), `:29` (UNIQUE) and `:51` (DROP allowlist).
- [ ] **The PAIR stored on a payment row is guarded too** (reopened 2026-10-03, closed the
      same day — see the follow-up section).
      The criterion above closes only the METHOD's side: it guarantees
      `payment_methods.account_id` is a single account, not that a payment row
      names it. `sale_payments` (and its siblings) store `account_id` and
      `method_id` as two independent NOT NULL columns, so the pair was consistent
      by caller discipline and not by the database — a direct SQL insert could name
      any account. Migration 44 adds the `BEFORE INSERT` guard on the three
      method-**choosing** tables; the two refund tables are deliberately exempt.
- [x] Collect, sale confirm/payment, purchase confirm/payment: solo método
      Closed 2026-10-02 **against the tree**: `customer_receipts.rs:105`, `sales.rs:1298,1542` and `purchases.rs:1092,1288,1353` take only the method; only `name="method_id"` selects appear in `customer_detail.html:52`, `sale_detail.html:187,250` and `purchase_detail.html:611,656`.
- [x] Combinación inválida imposible por construcción (sin 400 evitable)
      Closed 2026-10-02 **against the tree**: ownership at `payment_method_repo.rs:90`, fixture-only allowlist at `:408` with test `:465`, and the rule is stated at `sales.rs:1286`, `purchases.rs:1080` and `finance_methods.rs:8`.
- [x] `cargo test` full en verde, `cargo check` limpio
      Closed 2026-10-02 **against the tree**: CI `checks` success on HEAD `d876e2e`; `cargo check --all-targets` exits 0 with 0 errors and 75 warnings.
- [ ] README + spec finance actualizados
      Stays unchecked (2026-10-02) — PARTIAL: `README.md:178-199` documents it and the spec was updated at `98c7194`, but the `openspec/` tree was deliberately deleted in `76d0cbb`, so the spec half cannot be satisfied in the current tree — this box needs rewriting, not ticking.

## Applicable checks
- `cargo test` (full), `cargo check --all-targets`
- Migración probada en copia de `roya.db` real + fresh DB

## Tasks
- [x] T1 — Migración + repo + servicio + modelo
- [x] T2 — Servicios que consumen el par (receipts, sales, purchases)
- [x] T3 — REST + web + templates (solo-método en las 3 superficies)
- [x] T4 — Seeds/defaults con duplicación + tests + docs + verificación
- [x] **T5 — Migration 44: the `BEFORE INSERT` method→account guard on the three
  method-choosing payment tables** (reopened and closed 2026-10-03, see the
  follow-up section).
  `sale_payments`, `purchase_payments`, `customer_receipts` — the three where an
  operator actually picks a method. `customer_return_payments` and
  `purchase_return_payments` are **exempt**: a refund does not choose a pair, it
  replays the parent payment's pair. Insert-only on purpose: equality when the row
  is born, divergence allowed afterwards, because the stored account is the
  historical fact of where the money landed while the method is mutable
  configuration (`sales.rs:1751` already reads the stored account to refund, which
  is correct and must stay).
  Done 2026-10-03. Observed: `cargo test --locked` **1512 passed / 0 failed**;
  warnings exactly at the 79 bin / 49 test baseline (delta 0); `git diff --check`
  clean; `cargo fmt --check` clean; migrations 1–43 byte-identical. Red observed
  honestly: the five refusal tests failed with `unwrap_err()` receiving `Ok` (a
  mismatched pair, an unassigned method and a nonexistent method all inserted).
  Independent `gentle-ai-verify` confirmed the migration text, the absence of any
  Rust-side duplicate, that every other hunk in the 15-file diff falls inside a
  test module, and — the finding that mattered — that `INSERT OR REPLACE` cannot
  bypass the guard.
- [x] **T6 — Seed the default pair, so a fresh install can collect** (added
  2026-10-03 by the user: sembrar una cuenta "cash" y un método de pago "cash"
  linkeado a la cuenta, "lo mínimo, básico y calculo que obligatorio en cualquier
  negocio"). Today **no migration seeds an account** and `/setup` does not touch
  accounts or methods (`routes/setup_web.rs` references neither), so a brand-new
  installation has five seeded methods **all unassigned** and no account at all: it
  cannot record a single collection or supplier payment until someone creates an
  account and links a method by hand, because the services refuse an unassigned
  method. Seed a `Caja` account plus the seeded `Cash` method owned by it, in one
  migration, following the walk-in precedent
  (`migrations/20240101000020_create_customers.sql:31-34`: guarded
  `INSERT … SELECT … WHERE NOT EXISTS`) and taking `created_by` from the seeded
  `sistema` user (`…30:58-65`). Tests: a fresh database can collect after the
  migration; a re-run is a no-op; the seeded pair satisfies the migration-44 guard.
  Second half of T6, and a real gap rather than a nicety: **the name-keyed default
  assignment is test-only today.** `ensure_defaults_for_account` and
  `default_method_names_for_account_name` (Caja→Cash, Banco→Transfer/Debit/
  CreditCard, MP→QR/Transfer) have 24 call sites and **every one is inside a
  `#[cfg(test)] mod tests`**; production account creation is a bare
  `self.accounts.create(actor, trimmed)` (`services/account.rs:43`, wired at
  `routes/mod.rs:401`). A business adding its own `Banco` account therefore gets no
  methods — which contradicts what T4 of this document claimed to deliver. Decide:
  wire it into account creation (recommended — three lines at the call site, and
  "each business adds its own accounts and methods" becomes a single step), or drop
  the helper and leave linking manual.

  **UPGRADED 2026-10-03 (user decision): no orphans by design — and the design is
  the schema.** The user's rule is *ningún método puede quedar huérfano por diseño*,
  and the honest reading of it is stronger than the seed, because **the product
  produces orphans from a reachable screen today**: the account's method editor
  (`replace_account_methods`, routes `POST /web` at `routes/web.rs:480` and
  `PUT /api` at `routes/api.rs:103`) **unassigns** whatever is unticked
  (`services/finance_methods.rs:155` and `:205` call `set_method_account(…, None)`),
  and there is no method delete (`ON DELETE RESTRICT` from every payment table). So
  "quitar un método de una cuenta" only ever meant "leave it without an owner".

  The shape, therefore:

  1. **`payment_methods.account_id` becomes `NOT NULL`**, closing the allowance
     migration 24 documented (`…24:2-3`, "NULL = unassigned, not usable for
     payments"). SQLite needs the house table rebuild (the technique of
     `…24` and `…30`: `-- no-transaction` on the first line, foreign keys off around
     the swap, because `sale_payments`, `purchase_payments`, `customer_receipts` and
     the two return-payment tables reference it with `ON DELETE RESTRICT`).
     **A rebuild drops triggers: `trg_payment_methods_set_updated_at` (`…36`) must be
     recreated**, or every `updated_at` stamp on that table silently stops.
  2. **Seed `Caja` + `Cash`.** Find-or-create the account (`accounts.name` is
     `UNIQUE`, so a blind `INSERT` aborts on a database that already has one), take
     `created_by` from the seeded `sistema` user (`…30:58-65`), and attach the seeded
     `Cash` only if it has no account — the walk-in precedent
     (`…20:31-34`, guarded `INSERT … SELECT … WHERE NOT EXISTS`).
  3. **The remaining orphans.** One with **no payment history** is a seed leftover:
     delete it (on a wiped database that is `Transfer`, `Debit`, `CreditCard`,
     `QR`). One **with** history cannot be deleted: give it the default account so
     the schema holds **and set `is_active = 0`**, because it is a legacy artifact
     and must never be selectable. Neither path may lose a referenced row.
  4. **"Quitar un método de una cuenta" becomes DEACTIVATE.** `is_active = 0` — it
     already makes `resolve_account_for` refuse the method, so the operator sees a
     clear "inactive" refusal instead of a method that vanished into an unusable
     state. The repository loses the `None` arm: `set_method_account(actor, id,
     account_id: i64)` plus a `set_active(actor, id, bool)`, with every caller updated.
  5. **Wire the defaults into production account creation** — the second half of
     T6 above — so `Caja`/`Banco`/`MP` arrive with their methods and "each business
     adds its own accounts" is one step instead of two.

  Test rewrites this forces, named so nobody discovers them mid-flight:
  `seeded_methods_start_unassigned_without_other`,
  `the_public_find_method_answers_exactly_as_before_including_the_unassigned_method`,
  `set_method_account_assigns_unassigns_and_rejects_unknowns`, and the audit-stamp
  assertion in `src/t1_schema_tests.rs` that uses `set_method_account(…, None)` to
  provoke an `updated_at` write. Plus the 24 test call sites of
  `ensure_defaults_for_account`, which exist to mimic production and can now be
  deleted where the production path does the work.

  **The mirror case, decided rather than left implicit:** an **account** with no
  methods stays allowed and stays flagged
  (`web_create_account_without_ticked_methods_creates_flagged_account`,
  `routes/web.rs:1384`). That is an account-level state the operator can see and
  fix, not an orphan method sneaking out of service, and forbidding it would mean
  inventing a method for a business that has none. Say so if you disagree.

  Forecast: **~450 lines**, so T6 is a real slice and not a seed: migration +
  repository signature + editor semantics + account-creation wiring + the rewrites.

## T6 implementation log (2026-10-08) — CLOSED

**State: implemented, 1518 Rust tests green, 180 browser tests green, warnings at
the 79 bin / 49 test baseline (delta 0), `cargo fmt --check` clean, visual
baseline regenerated after proving the diff is only the intended one.** T6 is the
first slice of the payment chain; P1 of `odd/tasks/payment-allocation.md` is next.
**Committed as `f9b769a`** on `feat/party-ledger`, after the user authorized the
work-unit commit: 39 files, 2289 insertions, 704 deletions, one purpose.

Resumed from a writer that ran out of tokens. Its tree was kept: migration 45, the
repo service changes, its five T6 tests and the `db_err_message` helper. What was
added on resume, and why:

- **The migration never disabled foreign keys.** `PRAGMA foreign_keys = OFF;` was
  missing at the top while the first statement is an `INSERT` into a referenced
  table. Added, with the reason: the seed and the orphan resolution must not have
  correctness depend on statement order. The end of the file now decides instead,
  by measurement — a `pragma_foreign_key_check()` count folded into a temp-table
  `CHECK (violations = 0)`, so a broken swap aborts the migration rather than
  printing and continuing.
- **The rebuild's trigger recovery was wrong twice over.** The file dropped
  migration 44's three child guards *after* the parent drop; SQLite recompiles
  their bodies on the following `ALTER TABLE RENAME` while `main.payment_methods`
  is a dangling name, which is exactly how a product stops accepting payments.
  They are now dropped before the parent and recreated immediately after the
  rename. And `trg_payment_methods_set_updated_at` — which migration 36 installs
  with `BEGIN SELECT NEW.updated_at = strftime(...); END` — is **not recreated**:
  measured, that statement is a comparison, not an assignment, so it has never
  stamped anything on any table 36 touched. A table rebuild drops triggers, so
  the default on the new `updated_at` column replaces it. **Follow-up slice:** the
  same broken idiom remains on `transactions`, `categories`, `product_supplier_costs`,
  `sale_payments` and `purchase_payments`. Not fixed here — five tables with their
  own evidence is its own work unit.
- **The `RETURNING` trap is why the fix is a column DEFAULT.** A `BEFORE`-trigger
  fix would still read back the sentinel, because `RETURNING` reports the row as
  the INSERT left it in every case; an `AFTER INSERT` trigger writes the real value
  but `RETURNING` does not see it either. `create_in_account` uses `RETURNING`, so
  the only shape that is honest to its reader is the DEFAULT.
- **The `DELETE` in step (c) is not a new rule, it is a reachable end state.**
  `replace_account_methods` unassigns whatever is unticked, so a brand-new install
  could reach "every method unassigned" through a screen. Under `NOT NULL` that
  same history-less leftover is simply deletable; one that carries payment history
  is adopted by `Caja` and deactivated.
- **Scope added, discovered while resolving the failures: two UI surfaces die with
  the allowance.** `account_detail.html` renders an "Unassigned methods (select to
  assign)" block fed by `PaymentMethodService::unassigned()` (`routes/web.rs:291`),
  with its two `MessageKey`s. With `account_id NOT NULL` the list is permanently
  empty, so the block, the query, the service method and the keys are removed
  rather than left as a screen that always renders nothing. `AccountSaveHelp`
  likewise still promises "Saving replaces this account's method set" and is
  rewritten to the deactivate semantics in both catalogs.
- **`set_method_account` loses its `Option` arm.** Unticking now calls a new
  `set_active(actor, method_id, false)`; the repository can no longer write a NULL
  owner, so the type says so. The "belongs to another account" refusal stays but
  its message changes: "unassign it there first" is no longer something an operator
  can do.
- **Two asymmetries the work exposed, both fixed at the source.** The first:
  `POST /api/accounts` created a bare account while the web form wired the name's
  defaults, so `Caja` meant two different accounts depending on the surface — the
  DTO now carries `method_ids` and the handler routes them through the same
  `assign_or_duplicate` the form uses. The second: `PUT .../payment-methods`
  refuses a method another account owns, so a caller could create an account but
  never endow it; the creation path is the door that duplicates a name, and the
  API now offers it too. Neither is a test fix; both were latent.
- **`record_payment` now checks the (method, stated account) pair before writing.**
  Migration 44's trigger was the only thing refusing a mismatch, which surfaced as
  a raw SQLite abort — a 500 with driver text for an operator's mistake. The check
  asks the trigger's own question (`method.account_id = stated`) and answers 400.
  The pair is still derived from the method; nothing else changed.
- **`accounts_without_methods` counts ACTIVE methods, not rows.** With deactivation,
  an account whose every method was unticked still has rows while being exactly the
  account the warning is about. Counting rows would have silenced the warning at
  the moment it became true — the find the new test `replace_account_methods_accepts_empty_and_warns`
  produced.
- **The 133 red tests were measured, then repaired in two classes**: three upgrade
  fixtures inserting `sale_payments`/`customer_receipts` with a hardcoded
  `method_id = 1` through the pre-migration 44 schema (the run-44 trigger then
  refused the pair), and roughly 130 fixtures built on "create an account locally,
  reuse a seeded method" or on a raw balance insert that never ran the overdraft
  guard. The second class is the one worth remembering: those fixtures were
  asserting behaviour the guard had never enforced.

## Progress
- 2026-09-18: documento creado, rama `feat/payment-method-single-account`.
- 2026-10-03: **reopened for T5.** Reading the migration pair while designing the
  payment-allocation work surfaced that the DB-level claim was half true, exactly
  the shape of the party-ledger's decision 3 (append-only as prose until migration
  43). Migration 44 puts the guard in the schema.
- 2026-10-03: **T5 closed, with the scope narrowed once the guard was tested against
  the whole tree.** 1512/0 green. The canary produced 13 findings, every one a
  **test fixture** that had been encoding impossible states — twelve pairing a
  locally created account with an unassigned seeded `Cash` (or a hardcoded
  `method_id = 1`), and `src/t1_schema_tests.rs` pairing an account with a method
  the test itself had just unassigned. **Zero production findings**: every live
  writer resolves the account from the method, which is the evidence that the
  caller discipline this document described was real — and now the schema says it
  too. The 14th finding was structural and is the interesting one: an independent
  verifier proved that a *refund* is born mismatched when a method has been
  re-pointed, so the guard was aborting a valid operation. The two refund tables
  were exempted rather than changing where a refund comes from.
- 2026-10-03: **T6 opened, then upgraded the same day.** The user asked for the
  default pair to be seeded; gathering evidence showed the gap is wider (the
  name-keyed defaults are called only from test modules) and that the product
  creates orphans from the account-methods editor. The user then chose the strong
  reading of *ningún método huérfano por diseño*: `account_id NOT NULL` and
  "remove a method" becomes **deactivate**. T6 is consequently a full slice with a
  schema rebuild, and it is the **first** unit of this chain — before the
  payment-allocation schema (P1 of `odd/tasks/payment-allocation.md`), because a
  fresh install must be able to collect money before anything else is built on top
  of it.
  **CLOSED 2026-10-08 — committed as `f9b769a`.** Observed: `cargo test --locked`
  1518 passed / 0 failed; `scripts/e2e.sh` 180 passed / 0 failed; warnings at the
  79 bin / 49 test baseline (delta 0); `cargo fmt --check` and `git diff --check`
  clean; baseline visual regenerada con prueba; instalación fresca mirada en
  navegador. Ver la sección "T6 implementation log" para las tres trampas que el
  trabajo destapó.

## Verification evidence
- Writer: `cargo check --all-targets` 0 errores; `cargo test` full 320 passed
- Orquestador spot check: `cargo test` → 320 passed (1 suite)
- Migración verificada en copia de `roya.db` real: Cash→acc 1,
  Transfer/Debit/CreditCard→acc 2, QR→NULL; ids estables; historial
  intacto; `foreign_key_check` limpio
- Migración `20240101000024` debe mantener `-- no-transaction` en primera
  línea (sqlx + PRAGMA foreign_keys, verificado empíricamente)

## Follow-up (2026-10-03) — the method→account guard on the payment rows

**The gap, precisely.** `payment_methods.account_id` is the method's single
account (`UNIQUE(account_id, name)`, NULL = unusable). Every collection path
resolves the account from the method and then writes **both** columns:
`resolve_method_account` (`services/sales.rs:141-142`) → `create_payment_in`
(`sales.rs:1471`, `:1580`). The module states the intent — *"Payments name only
the method; the account is derived from ownership, so an invalid combination is
impossible by construction"* (`services/finance_methods.rs:1-8`) — but
"by construction" here means **by caller discipline**. No constraint, index or
trigger relates `sale_payments.account_id` to
`payment_methods.account_id`; a raw insert can pair account 3 with a method owned
by account 7, and nothing refuses it.

**The guard (migration 44).** One `BEFORE INSERT` trigger per table, no
`-- no-transaction` needed:

```sql
CREATE TRIGGER IF NOT EXISTS trg_sale_payments_method_account_insert
BEFORE INSERT ON sale_payments
FOR EACH ROW
WHEN NEW.account_id <> COALESCE(
        (SELECT account_id FROM payment_methods WHERE id = NEW.method_id), -1)
BEGIN
    SELECT RAISE(ABORT, 'the payment method does not belong to the named account');
END;
```

Three details that are load-bearing, not incidental:

1. **`COALESCE(…, -1)` is not decoration.** With the bare comparison, an
   *unassigned* method (`account_id IS NULL`) makes the subquery NULL, the `<>`
   NULL, and `WHEN NULL` means **no abort** — the exact case the guard exists to
   stop would pass silently. The `-1` sentinel also refuses a `method_id` that
   does not exist, which makes the trigger stronger than the FK (it holds even
   where `PRAGMA foreign_keys` is off).
2. **Insert-only, deliberately asymmetric.** Equality is enforced when the row is
   born; afterwards the stored account is a frozen fact. `sales.rs:1751` refunds
   to `pay.account_id`, and that is correct: if an owner re-points "Transfer" to
   another account, history must not move. A `BEFORE UPDATE` twin would block
   that legitimate divergence.
3. **`sale_payments.method_id` has `DEFAULT 1`.** Any seed or fixture that
   inserts only `account_id` silently pairs it with method 1, whose owner may be
   another account — so this guard **will** surface existing inconsistent writers
   and fixtures. That is the point: treat each hit as a finding to fix and report,
   never as an obstacle to route around.

**Requirement for the payment-allocation unit.** When the `payments` document
lands (`odd/tasks/payment-allocation.md`), it inherits this same guard, because it
carries the same pair for the same reason. And the schema stays the only source of
the rule: no caller-side duplicate of the check.

### Scope narrowed: the two refund tables are exempt (2026-10-03)

The guard was first written for all five payment tables. Independent verification
found the counterexample that killed that scope, and it is worth keeping written:

**`RefundPlan` replays the parent payment's pair on purpose.**
`src/services/customer_return.rs:707-708` and
`src/services/purchase_return.rs:723-724` build
`RefundPlan { account_id: pay.account_id, method_id: pay.method_id, .. }`, and the
refund row is inserted with exactly that pair (`customer_return.rs:648`,
`purchase_return.rs:663`). The intent is that a refund's cash comes back out of
the box the payment went into. But a method can be re-pointed
(`set_method_account` / `replace_account_methods`, `finance_methods.rs:143-165`,
reachable from the accounts UI), and once it has been, a **legitimate** refund row
is born mismatched against the method's *current* owner. The guard aborted it —
before migration 44 that refund committed. Reproduced end to end through the real
confirm path, not just with raw SQL.

**Resolution.** The rule is about a pair being **chosen** at birth. A refund does
not choose; it replays the pair of a row that was itself guarded at birth, so on
those two tables the guard protected nothing while breaking a valid operation. A
hardening that changes behaviour for a valid operation is wrong, not the operation.
The guard now covers the three method-choosing tables, and the exemption is pinned
by four tests, each observed failing (refused) while the two triggers still
existed, so none of them is a tautology: one per refund repo
(`a_credit_note_refund_row_is_exempt_from_the_method_account_guard`,
`a_purchase_return_refund_row_is_exempt_from_the_method_account_guard`) and one per
return service
(`a_refund_replays_the_parent_payments_account_even_after_the_method_is_repointed`),
the last two asserting the historical account on **both** the refund row and its
finance transaction.

**Accepted residual hole, named rather than hidden.** A direct SQL insert into
`customer_return_payments` or `purchase_return_payments` can still pair any
account with any method. The reason it is acceptable: their only writer copies a
pair from a parent row that *is* guarded, so getting it wrong requires writing SQL
by hand — and the two exemption pins make the intent explicit to the next reader.

**Rejected alternative, recorded so it is not rediscovered as an oversight.**
Making refunds resolve the account from the method's current owner would close the
hole entirely, at the price of changing *which box funds a refund* the moment
anyone re-points a method. That is a product decision about money movement, not a
hardening side effect, so it is not taken here. If the owner ever wants it, it is
a one-line change in each `refund_plan` plus a rewrite of the four pins above.

**Why not simply make the guard permissive on those two tables** (accept any pair
that matches *some* historical payment): it cannot be expressed as a `WHEN` clause
without joining the payment tables in the trigger, and it would encode a rule
(`the pair must have existed before`) that nothing else in the schema can check —
more cleverness than the invariant is worth, for two tables whose writers already
copy a validated pair.

### The defaults this document claimed, and where they actually live (2026-10-03)

T4 says "Seeds/defaults con duplicación + tests + docs + verificación", and the
acceptance criterion says an invalid combination is impossible by construction. The
**rule** is enforced (migration 44, plus the services refusing an unassigned
method). The **defaults** are not: `grep -rn ensure_defaults_for_account src/`
returns 24 call sites and **not one is outside a `#[cfg(test)] mod tests`**.
Production account creation is `self.accounts.create(actor, trimmed)`
(`services/account.rs:43`), reached from `routes/mod.rs:401`, with no method
assignment anywhere on that path.

Visible in the product, not only in the tests:

- A fresh installation has **no account at all** and five unassigned methods, so
  nothing can be collected or paid until both are configured by hand.
- Creating an account named `Banco` or `Caja` does **not** give it the default
  methods the helper defines, so every account starts as manual configuration.
- The dev database at the repository root carries an account literally named `Cash`
  with method `Cash` linked, and `default_method_names_for_account_name` only knows
  the exact names `Caja`, `Banco` and `MP` — so even the helper would not have fired
  for it. That pair was linked by hand.

T6 covers the seed and the wiring decision.

## Next step
- Pulido del drawer HECHO (3 cards, contraste, botón editar por fila).
  Revisar en navegador y commitear (sin commitear por ahora).

## Drawer polish (2026-09-18, misma rama)
- Drawer customer en 3 cards (cabecera / collect solo-método / documentos);
  supplier en 2 (sin form de pago). `customer_statement.html` recortado a
  cabecera+saldo+ageing + línea de notes.
- Contraste: la causa era el estilo base de `button` (texto oscuro sobre
  fondo accent); filas ahora usan `text-text`/`text-muted`. Sin rebuild CSS.
- Edit por fila: botón ✎ abre `<dialog>` con form precargado
  (`GET /web/customers/edit-form/{id}`, `GET /web/suppliers/{id}/edit-form`);
  backends de update sin cambios. Div con dos botones hermanos (sin
  button-anidado). Spot check: `cargo test` → 322 passed.
