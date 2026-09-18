# Rediseño Customers / Suppliers — lista mínima + drawer lateral

## Objective
Simplificar las pantallas de customers y suppliers: listas de solo nombres,
alta por botón+modal, sin card de edición, y detalle en slide-over derecho
con cabecera, saldo y documentos asociados.

## Problem
Las páginas muestran info irrelevante (ageing, límites, costos, IDs) y las
cards de New/Edit ocupan la columna lateral de forma permanente.

## Why
Lectura rápida del catálogo y detalle bajo demanda; alta sin ruido visual.

## Scope
- `templates/customers.html`, `templates/suppliers.html`
- `templates/partials/customer_list.html`, `templates/partials/supplier_list.html`
- Nuevo: `templates/partials/customer_detail.html`,
  `templates/partials/supplier_detail.html` (drawer)
- `src/routes/customers_web.rs`, `src/routes/suppliers_web.rs`
  (fragmento detalle supplier + balance; reutilizar statement en customer)
- Tests que asertan el layout viejo
- Fuera de alcance: cambiar reglas de negocio, saldos, API REST

## Constraints
- HTMX + Askama, sin JS framework; `<dialog>` para alta, panel fijo derecho
  para el drawer
- Formularios de cobro (customer) y de costos (supplier) se conservan pero
  viven en el drawer / modal según corresponda, no como cards fijas
- El endpoint de update (`/web/customers/edit`, `/web/suppliers/edit`) se
  conserva a nivel backend; solo se retira la card visible

## Authorized scope
Rediseño visual + fragmento detalle supplier. Sin cambios de negocio.

## Acceptance criteria
- [ ] Lista customers y suppliers muestra solo nombres (clickeable)
- [ ] Botón "New customer/supplier" abre modal con el form de alta
- [ ] Sin card de edición visible en ninguna de las dos páginas
- [ ] Click en un nombre abre slide-over derecho: cabecera con datos,
      saldo, documentos asociados (ventas / compras)
- [ ] `cargo test` relevante en verde (customers_web, suppliers_web)

## Applicable checks
- `cargo test customers_web suppliers_web` (o el filtro que corresponda)
- `cargo check` si se tocan routes

## Tasks
- [x] T1 — Customers: lista solo-nombres + botón/modal alta + drawer
      (cabecera, saldo, ventas). Quitar card Edit.
- [x] T2 — Suppliers: lista solo-nombres + botón/modal alta + drawer
      (cabecera, saldo = suma due confirmado, compras). Quitar card Edit.
      Nuevo `GET /web/suppliers/{id}/detail`.
- [x] T3 — Actualizar tests del layout viejo + verificación final.
- [x] T5 — Card "Pay supplier" en drawer (espejo de Collect payment),
      arriba de Record Product Cost: `pay_supplier` con allocation
      oldest-first, sin recibo agrupador (suppliers no tienen ese
      documento).

## Progress
- 2026-09-17: documento creado, rama `feat/redesign-customers-suppliers`.
- 2026-09-18: todo commiteado junto con la migración en `ee52292`
  (un solo commit: las firmas nuevas y las rutas viejas no compilaban por
  separado). Suppliers con drawer de 3 cards: Record Product Cost vive en
  el drawer (supplier fijo, hidden field); `web_record_cost` ramifica por
  header `HX-Target` y emite `supplier-cost-recorded`. Spot check:
  `cargo test` → 324 passed.

## Verification evidence
- `cargo test` full: 315 passed, 0 failed (spot check del orquestador)
- `cargo check --all-targets`: 0 errores
- Probe en vivo: `/customers`, `/suppliers` → 200 con layout nuevo;
  fragmentos detalle rinden cabecera/saldo/docs
- T5 (2026-09-17): `POST /web/supplier-payments` (forma en el drawer,
  `HX-Target` ramifica al fragmento fresco, trigger `supplier-paid`) +
  `POST /api/supplier-payments` (pagos creados en JSON). `cargo test` full:
  330 passed, 0 failed (4 tests `t5_pay_supplier_*` + web + REST nuevos);
  `cargo check --all-targets`: 0 errores.

## Desviación registrada
- Filas customer usan `/web/customers/detail/{id}` (id-final) en vez del
  `/web/customers/{id}/statement` propuesto: el wiring guard de
  `smoke_tests` rechaza ids concretos a mitad de path. Mismos datos,
  guard intacto.

## Next step
- Revisar en navegador y commitear en la rama
  `feat/payment-method-single-account` (pendiente: cost-drawer + pay-supplier
  sin commitear, `cargo test` → 330 passed).
