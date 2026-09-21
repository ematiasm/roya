# Margen por producto → precio de venta derivado

## Objective
Un campo de margen por producto que **calcule** el precio de venta, para dejar de
tipear el precio a mano en cada alta y evitar que quede despegado del costo.

Fórmula decidida: `sale_price = cost_price × (1 + markup_pct/100)` — **markup
porcentual sobre costo**, no margen sobre precio de venta.

## Problem
`products` tiene `sale_price` y `cost_price` como dos valores manuales e
independientes (`migrations/20240101000004_create_products.sql:10-11`,
sostenidos en el rebuild de `migrations/20240101000031_add_audit_inventory.sql:106-107`).
No existe relación entre ellos: `validate_product` valida cada uno por separado
(`src/services/inventory.rs:209-228`) y el campo de margen no existe en esquema,
modelo, UI, spec ni lógica de venta. El operador calcula el markup a mano o no lo
calcula.

## Why
El precio de venta es una decisión que depende del costo, y hoy el sistema trata
las dos como datos sueltos sin vínculo. Con el margen como dato, el precio se
recalcula solo y el porcentaje aplicado deja de ser conocimiento tribal.

## Decisiones del usuario
- **Semántica**: markup porcentual sobre costo, `precio = costo × (1 + m/100)`.
  Se evaluó margen sobre precio de venta (`costo / (1 − m/100)`) y se descartó:
  exige división, y el repo **nunca dividió un `Decimal`** — no hay `round_dp`,
  `RoundingMode` ni `checked_div` en ningún lado del código.
- **Quién manda**: el margen manda. El precio se deriva al guardar el producto y
  pasa a ser de solo lectura en la UI cuando hay margen cargado.
- **Qué costo**: el campo `products.cost_price` que ya existe en la tabla. Se
  evaluó derivar del costo de referencia del satélite
  (`product_supplier_costs`) y se descartó: es **por proveedor**, así que con dos
  proveedores el precio de venta queda ambiguo.
- **Cascada**: cuando `cost_price` cambia y hay markup cargado, el precio de
  venta **se recalcula solo**, en el mismo acto. El humano aprueba el costo, el
  precio sigue de forma determinista.
- El historial está a salvo por diseño: `sale_lines.unit_price` copia el precio
  al armar la línea (`src/services/sales.rs:319-325`), no lo referencia.

## Dependencia con la feature hermana (F2)
Esta feature deriva el precio de `cost_price`, y `cost_price` **hoy nadie lo
mantiene**: las compras nunca lo escriben — actualizan el satélite
`product_supplier_costs`, y la spec lo declara *"the fallback for products with
no supplier row"* (`openspec/specs/purchases/spec.md:47`, ver
`src/services/suppliers.rs:9-11`). O sea: el markup derivaría de un costo que
puede estar viejo.

Eso se resuelve en `odd/tasks/cost-price-freshness.md` (F2), no acá. Decisión ya
tomada: **solo un botón explícito escribe `cost_price`**, así que la invariante
AC10 (`openspec/specs/purchases/spec.md:46`, test
`src/services/purchases.rs:2123`) sobrevive intacta.

**Restricción de forward-compat que esta feature debe respetar**: la derivación
vive en `validate_product` (`src/services/inventory.rs:189`), que es la única
entrada de validación de create y edit. Para que el botón de F2 recalcule el
precio sin duplicar la fórmula, F2 tiene que escribir `cost_price` pasando por
`update_product` con un patch de `cost_price` (`src/services/inventory.rs:337`),
no con un UPDATE directo. La fórmula queda en un solo lugar.

## Scope
Fuera de alcance: margen por categoría, margen global de configuración, historial
de precios, y cualquier cambio en la lógica de venta (el precio de línea sigue
naciendo igual).

Backend
- `migrations/20240101000035_add_product_markup.sql` (nuevo) —
  `ALTER TABLE products ADD COLUMN markup_pct TEXT NULL;`. ADD COLUMN pelado: es
  la convención del repo para nullable con default NULL, sin rebuild y sin
  `-- no-transaction` (`migrations/20240101000019_link_payments_to_transactions.sql:14`,
  `migrations/20240101000023_add_sale_payments_receipt.sql:6-7`; la regla está
  escrita en `migrations/20240101000034_add_audit_identity_tables.sql:112-122`).
  Version siguiente a la más alta (34).
- `src/models.rs` — `markup_pct` en `Product` (`:289`), `NewProduct` (`:360`) y
  `UpdateProduct` (`:380`). En `UpdateProduct` va como `Option<Option<Decimal>>`
  (misma semántica que `location`/`notes`, doc en `:378-379`), porque **NULL es
  un valor con significado** ("sin margen") y no "sin cambio".
- `src/repositories/product_repo.rs` — columna en los **8 statements** que listan
  columnas (create `:110-134`, find_by_id `:137`, find_by_sku `:147`,
  find_by_sku_ci `:157`, list `:184`, list_by_category `:193`, update `:225-253`,
  **set_active `:212`**), más `row_to_product` (`:53-72`) y el bind del patch.
- `src/services/inventory.rs` — derivación en `validate_product` (`:189`), que es
  la **única** entrada de validación para create y edit; más el merge del patch
  en `update_product` (`:339-352`) que tiene que fusionar `markup_pct` **antes**
  de derivar.
- `src/routes/inventory_api.rs` — DTOs `CreateProductRequest` (`:44`) y
  `UpdateProductRequest` (`:79`, con el visitor `double_option` de `:74-77`) y su
  mapeo (`:217-230`, `:256-270`).

UI
- `src/routes/inventory_web.rs` — los dos gates `"sale_price is required"`
  (`:708` create, `:863` edit) tienen que volverse condicionales, o un producto
  con margen no se puede guardar. Form structs `CreateProductForm` (`:519`) y
  `EditProductForm` (`:561`); el edit siempre manda todos los campos y arma
  `Some(...)` (`:878-903`).
- `templates/products.html` — input de margen en el modal; `sale_price` (`:137`)
  pierde `required` y queda readonly cuando hay margen.
- `templates/partials/product_detail.html` — input de margen en el drawer;
  `sale_price` (`:100`) idem; el header muestra `${{ product.sale_price }}`
  (`:31`).

Spec
- `openspec/specs/inventory/spec.md` — **contradicción dura**: `:16-18` enumera
  las columnas de `products` y `:29-31` las reglas de precio. Hace falta un
  change folder (`openspec/changes/<name>/`) con `proposal.md`, `spec.md`
  (delta + AC), `design.md` (con tradeoffs) y `tasks.md`, y después la promoción
  (`openspec/specs/README.md:1-8`). Artefactos OpenSpec **en inglés**
  (invariante 6).

## Constraints
- Sin `round_dp`/`RoundingMode`/`checked_div` en todo el repo: la política de
  escala es nueva, no heredada.
- `parse_decimal_opt` (`src/repositories/product_repo.rs:41-43`) mapea un valor
  corrupto a `Decimal::ZERO`, **no** a `None`. Reusarlo para `markup_pct` haría
  que un dato podrido se lea como "0% de markup" en vez de "manual": hace falta
  un parser dedicado que preserve `None`.
- Los templates imprimen `Decimal` por `Display` sin capa de formato
  (`product_detail.html:31`, `product_list.html:18`,
  `product_search_results.html:123,129`), así que un valor derivado puede
  renderizar `$13.5` en vez de `$13.50`.
- strict_tdd: true (`openspec/config.yaml`); sin CI, las suites corren local.
- Cambiar `NewProduct` rompe los ~30 literales de fixture que no usan
  `..product_input(...)` (seeds en `services/sales.rs`, `purchases.rs`,
  `suppliers.rs`, `customer_receipts.rs`, `product_repo.rs` y varios route tests).

## Acceptance criteria
- [ ] `products.markup_pct TEXT NULL` existe; NULL = sin margen, precio manual.
      Las 3 filas existentes quedan en NULL sin que ningún precio cambie.
- [ ] Con `markup_pct` cargado, `sale_price` se deriva como
      `cost_price × (1 + markup_pct/100)` y se persiste.
- [ ] Con `markup_pct = NULL`, `sale_price` sigue siendo manual y editable.
- [ ] `cost_price = 0` (o NULL) con margen cargado **no** calcula: conserva el
      precio y falla con `AppError::Validation`, sin escribir un precio 0.
- [ ] `markup_pct` fuera de rango (precio resultante ≤ 0, o `≤ -100`) se rechaza
      con `AppError::Validation` en el estilo del repo (inglés, minúscula, sin
      punto final).
- [ ] Un valor corrupto en `markup_pct` se lee como `None`, no como 0%.
- [ ] En la UI, con margen cargado el precio es de solo lectura y el guardado
      funciona (el gate `sale_price is required` no lo bloquea).
- [ ] El precio derivado renderiza con 2 decimales (`$13.50`, no `$13.5`).
- [ ] `openspec/specs/inventory/spec.md` refleja la columna y la regla derivada.
- [ ] `cargo test` verde y `cargo check --all-targets` sin errores.
- [ ] `scripts/e2e.sh -k products` verde.

## Applicable checks
- `cargo test` (runner del repo), `cargo check --all-targets`
- `scripts/e2e.sh -k products`
- `scripts/build-css.sh` si cambian clases en los templates

## Tasks
- [x] T1 — Migración `20240101000035_add_product_markup.sql` + header comment.
- [x] T2 — Modelo: `markup_pct` en `Product`/`NewProduct`/`UpdateProduct` + churn
      de los literales de fixture (35 sitios, 14 archivos).
- [x] T3 — Repo: columna en los 8 statements + `row_to_product` + parser
      dedicado que preserva `None` + bind del patch. → `942e328`
- [x] T4 — Servicio: derivación en `validate_product`, bounds del markup, guard
      de costo 0, merge en `update_product` antes de derivar, y redondeo a
      centavos del valor derivado (el proyecto no tenía `round_dp`).
- [x] T5 — API: DTOs y mapeo en `inventory_api.rs`. → `1327a10`
- [ ] T6 — UI web: gates condicionales en los dos handlers + inputs de margen y
      precio readonly en los dos templates.
- [ ] T7 — Formato de display del dinero en templates (el redondeo del valor
      almacenado es parte de T4): hoy imprimen `Decimal` por `Display`, así que
      un derivado puede renderizar `$13.5` en vez de `$13.50`.
- [ ] T8 — E2E y smoke: cubrir el hueco — **ningún smoke test postea
      `/web/products/edit`** hoy (solo el route test de
      `src/routes/inventory_web.rs:1906` y e2e).
- [ ] T9 — Spec: change folder OpenSpec + promoción de la spec de inventory.
- [ ] T10 — Verificación final: `cargo test`, `cargo check --all-targets`,
      `scripts/e2e.sh -k products`.

## Progress
- Baseline: `cargo test` → 745 passed antes de T1.
- T1–T3 → `942e328`. `cargo check --all-targets` 0 errores; `cargo test` 745
  passed, 0 failed. Persistencia sola: nada computa con `markup_pct`, así que
  ningún precio almacenado cambió y la suite quedó idéntica en comportamiento.
- Verificación independiente (no la del writer): CONFIRMED en migración, los 8
  statements, los binds, el parser estricto y la disciplina de alcance. Se probó
  la migración aparte aplicando los 35 archivos a una DB temporal (35 ok) y
  `PRAGMA table_info(products)` devuelve `markup_pct TEXT notnull=0`.
- Correcciones que salieron de esa verificación: `smoke_tests.rs` **no**
  necesitaba churn (construye productos vía helpers de ruta, no por literal), y
  los binds de `create`/`update` son 14 y 15 (14 `SET` + 1 `WHERE id`), no 14 y
  14 como había reportado el writer. Ninguna de las dos es defecto.
- Nota de semántica para T4: `ProductRepository::update` recibe `&NewProduct`
  (fila completa, no patch), así que la distinción `Some(None)` = limpiar vs
  `None` = sin cambio vive en el merge del servicio
  (`src/services/inventory.rs:351`), no en el repo. Hay que testearla ahí.
- T4–T5 → `1327a10`. `cargo test` 764 passed, 0 failed. La fórmula quedó en
  `validate_product` (`src/services/inventory.rs`), con el porcentaje aplicado
  como multiplicación por `Decimal::new(1, 2)` — el repo sigue sin una sola
  división de `Decimal`, verificado con búsqueda sobre los 63 `.rs`.
- **Corrección a lo que yo había escrito**: en `rust_decimal` el enum es
  `RoundingStrategy`, no `RoundingMode` (confirmado en la crate 1.43.0,
  `decimal.rs:145`). Es la primera vez que el proyecto redondea.
- Verificación independiente de T4–T5: CONFIRMED en fórmula, orden de la regla
  de precio contra el precio efectivo, camino de recómputo por patch de costo,
  wiring de `double_option` en la API y disciplina de alcance. Encontró **seis
  huecos de cobertura**, todos cerrados en `1327a10`: redondeo a cero (producto
  rechaza, servicio acepta 0.00), aceptación de `m = -99`, `sale_price` sin
  sentido ignorado cuando hay markup, precio manual sin redondear, y el
  empate del redondeo (`10.005 → 10.01`, que es lo único que realmente prueba
  la estrategia half-up elegida).
- Divergencia deliberada y pineada: un `Product` cuyo precio derivado redondea a
  `0.00` se rechaza, pero un `Service` lo acepta, porque la regla preexistente
  permite servicio gratis (`sale_price >= 0`). Es consistente con la spec; queda
  como decisión visible, no como accidente.
- Baseline real de la suite: 745 → 758 (T4–T5) → 764 (hardening). El documento
  decía 745 "antes de T1" y el baseline previo a la feature era 745.
