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
