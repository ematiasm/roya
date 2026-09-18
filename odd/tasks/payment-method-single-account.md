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
- [ ] Un método pertenece a ≤1 cuenta a nivel DB (FK + UNIQUE)
- [ ] Collect, sale confirm/payment, purchase confirm/payment: solo método
- [ ] Combinación inválida imposible por construcción (sin 400 evitable)
- [ ] `cargo test` full en verde, `cargo check` limpio
- [ ] README + spec finance actualizados

## Applicable checks
- `cargo test` (full), `cargo check --all-targets`
- Migración probada en copia de `roya.db` real + fresh DB

## Tasks
- [x] T1 — Migración + repo + servicio + modelo
- [x] T2 — Servicios que consumen el par (receipts, sales, purchases)
- [x] T3 — REST + web + templates (solo-método en las 3 superficies)
- [x] T4 — Seeds/defaults con duplicación + tests + docs + verificación

## Progress
- 2026-09-18: documento creado, rama `feat/payment-method-single-account`.

## Verification evidence
- Writer: `cargo check --all-targets` 0 errores; `cargo test` full 320 passed
- Orquestador spot check: `cargo test` → 320 passed (1 suite)
- Migración verificada en copia de `roya.db` real: Cash→acc 1,
  Transfer/Debit/CreditCard→acc 2, QR→NULL; ids estables; historial
  intacto; `foreign_key_check` limpio
- Migración `20240101000024` debe mantener `-- no-transaction` en primera
  línea (sqlx + PRAGMA foreign_keys, verificado empíricamente)

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
