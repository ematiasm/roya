# Rediseño Products — botones + modales + drawer editable con costos por proveedor

## Objective
Llevar `/products` al mismo patrón que customers/suppliers: lista escaneable,
alta por botón + modal, y detalle bajo demanda en slide-over derecho. El drawer
del producto trae los campos editables en línea, los costos por proveedor
(registrar/actualizar, marca de preferido) y el movimiento de stock.

## Problem
La columna derecha tiene cuatro cards permanentes (New Category, New Product,
Stock Movement, REST API) que ocupan espacio fijo y obligan a tipear IDs a mano
para mover stock. No existe forma de editar un producto: no hay `UpdateProduct`,
ni `ProductRepository::update`, ni endpoint PUT. Los costos por proveedor solo
se ven desde el lado supplier.

## Why
Catálogo rápido de leer, alta sin ruido visual, edición del producto y de sus
costos por proveedor en el contexto del producto (que es donde se piensan).

## Decisiones del usuario
- Stock Movement pasa al drawer del producto (producto fijo) y se retira la card
  REST API de la página. Los endpoints REST siguen existiendo.
- La edición del producto es un form inline dentro del drawer, con guardado
  in-place (mismo patrón que Record Product Cost en el drawer de suppliers).

## Scope
- `src/models.rs` — `UpdateProduct` (patch).
- `src/repositories/product_repo.rs` — `update`.
- `src/services/inventory.rs` — `update_product` (reusa `validate_product`).
- `src/routes/inventory_api.rs` — `PUT /api/products/{id}`.
- `src/routes/inventory_web.rs` — fragmento del drawer + acciones web.
- `templates/products.html`, `templates/partials/product_list.html`,
  nuevo `templates/partials/product_detail.html`.
- `src/smoke_tests.rs` — el guard que asertaba page_header en `/products`.
- `e2e/tests/test_products.py` (nuevo, espejo de `test_parties.py`).
- Fuera de alcance: reglas de negocio (validaciones, prioridad del costo de
  referencia, allocations), otros módulos, y el borrado de endpoints REST.

## Constraints
- HTMX + Askama, sin framework JS. `<dialog>` para alta, panel fijo derecho
  para el drawer (`#product-drawer` / `#product-drawer-body`).
- Los IDs concretos van al final del path (`/web/products/detail/{id}`): el
  wiring guard de `smoke_tests` rechaza ids a mitad de path.
- Los errores viajan como JSON (`AppError`); el listener global de `base.html`
  los muestra con el nombre del form (`data-action`). Los forms del drawer
  llevan `data-action`.
- Validaciones del servicio intactas: `update_product` reusa `validate_product`
  y solo agrega el chequeo de SKU duplicado.
- Decimal-as-TEXT, montos como `Decimal` en Rust y `step="0.01"` en el form.

## Authorized scope
Rediseño visual + camino de edición de producto + superficie de costos por
proveedor desde el producto. Sin cambios de reglas de negocio.

## Acceptance criteria
- [ ] Alta de categoría y de producto por botón + modal; sin cards permanentes
      "New Category" / "New Product" ni card REST API.
- [ ] Click en un producto abre el drawer derecho con stock derivado, badges y
      el form editable completo; guardar refresca el drawer y la lista.
- [ ] El drawer lista los costos por proveedor (costo actual, previo, alerta de
      precio, preferido) y permite registrar/actualizar un costo y marcar
      preferido.
- [ ] El movimiento de stock se registra desde el drawer con el producto fijo.
- [ ] `PUT /api/products/{id}` con semántica patch (`None` = sin cambio,
      `Some(None)` = limpiar).
- [ ] `cargo test` verde y `cargo check --all-targets` sin errores.
- [ ] `scripts/e2e.sh -k products` verde.

## Applicable checks
- `cargo test` (runner del repo), `cargo check --all-targets`
- `scripts/e2e.sh -k products`

## Tasks
- [x] T1 — Backend de edición: `UpdateProduct`, `ProductRepository::update`,
      `InventoryService::update_product`, `PUT /api/products/{id}` + tests
      (service, repo, API).
- [x] T2 — Web: `GET /web/products/detail/{id}`, `POST /web/products/edit`,
      `POST /web/product-costs`, `POST /web/product-costs/preferred`, rama
      drawer en `POST /web/stock-movements`, acciones de ciclo de vida
      (`/web/products/activate|deactivate|delete`) + `product_detail.html`
      + tests de rutas.
- [x] T3 — Templates: `products.html` (header estilo parties + 2 modales +
      drawer), `product_list.html` (filas clickeables); actualizar el guard de
      `smoke_tests`.
- [x] T4 — E2E browser: `e2e/tests/test_products.py` (8 tests + probe de
      screenshots).
- [x] T5 — Verificación: `cargo test` 350 passed; `cargo check --all-targets`
      0 errores; `scripts/e2e.sh` 53 passed, 4 skipped (probes opt-in).
- [x] T6 — Fixes del bug reportado por el usuario (ver sección Bug fixes).
- [x] T7 — Refactor: la barra de filtros de products/sales/purchases deja de estar
      duplicada y pasa a un shell compartido
      (`partials/list_filters_open.html` + `list_filters_close.html`).

## Progress
- 2026-09-18: documento creado. Baseline `cargo test` → 330 passed.
  Trabajo sin commit sobre `main` (el repo venía en `main` limpio).
- 2026-09-18: T1 (341 passed), T2 (+6 tests, 347), T3 (+1 test, 348) completos.
- 2026-09-18: T4 e2e `-k products` → 8 passed; suite completa 50 passed.
- 2026-09-18: verificación independiente abrió dos gaps de cobertura (labels
  `data-action` del drawer sin pin, y limpiar campos opcionales por el form web
  sin test) y un defecto real: `Option<Option<T>>` + serde colapsa `null`
  explícito en el `None` externo, así que `PUT /api/products/{id}` no podía
  limpiar campos. Cerrado con `double_option` en `UpdateProductRequest`.

## T7 — Barra de filtros compartida (2026-09-18)

El shell de filtros estaba copiado en tres páginas y ya había divergido dos veces
(el `hx-include` faltante en products y el trigger `changed` muerto en las tres).
Se extrajo a `partials/list_filters_open.html` (apertura del form + contrato) y
`partials/list_filters_close.html` (Clear + Refresh dentro del form + cierre).
Cada página declara `filter_module` / `filter_list` y conserva su bloque de campos
entre los dos includes.

Por qué open/close y no un partial único con ramas: Askama compila **todas** las
ramas de un partial incluido contra el struct de **cada** página que lo incluye, y
los campos de filtro de las tres son disjuntos (`filter_q`/`filter_category` vs
`filter_status`/`filter_customer` vs `filter_status`/`filter_supplier`): el partial
único no compila (E0609). Partirlo deja el shell escrito una sola vez y los campos
—que son los data bindings de cada página— donde se leen.

El Refresh ahora vive dentro del form con `hx-include` explícito y
`type="button"`; se borraron los botones sueltos (fila de título de products y
headers de card de sales/purchases).

Equivalencia probada por comparación atributo a atributo del form renderizado
(3 páginas × estado vacío y filtrado): idéntico salvo el Refresh movido. Sin
cambios de comportamiento.

Queda explícita y a propósito una lista de módulos en `templates/base.html:132`
(el rewrite de `/web/(sales|products|purchases)?…` a `/<page>?…`): generalizarla
reescribiría cualquier fragmento futuro `/web/<x>` a un path que puede no ser
página.

Evidencia: `cargo test` 350 passed; `scripts/e2e.sh` 53 passed, 4 skipped; tests
e2e de layout del Refresh actualizados (rojo→verde) sin debilitar las
aserciones de comportamiento.

## Verification evidence
- `cargo test`: 350 passed, 0 failed (baseline 330; +20 tests nuevos).
- `cargo check --all-targets`: 0 errores, 51 warnings (pre-existentes).
- `scripts/e2e.sh`: 53 passed, 4 skipped (los 4 son probes opt-in).
- Probe visual: `ROYA_E2E_PRODUCTS_SCREENSHOT_PROBE=1 scripts/e2e.sh -k products`
  → 9 passed; screenshots en `e2e/.artifacts/design/07..11-products-*.png`
  revisados a ojo (drawer con form inline, costos por proveedor y movimiento).

## Bug fixes (post-reporte del usuario)

Reporte: "actualicé el precio coste del Agua 500ml y al guardar desaparece de la
lista; tengo que hacer refresh para que aparezca. Además Save tiene que cerrar el
side si se guardó bien".

Diagnóstico con evidencia de browser real (no reproducía en el caso simple:
eran tres defectos combinados):
1. **El filtro no filtraba al tipear.** `hx-trigger="change, keyup changed
   delay:300ms"` nunca dispara: el modificador `changed` de htmx 1.9.12
   (`static/htmx.min.js`) inicializa `lastValue` con el valor del elemento que
   lleva el trigger — el `<form>`, cuyo `.value` es `undefined` — y lo compara
   contra ese mismo `undefined`, así que aborta en cada tecla. Probe en browser:
   `keyup changed delay:300ms` → 0 requests; `keyup delay:300ms` → 1; `input`
   → 4. El filtro se aplicaba recién al perder foco (`change`), cuando el
   usuario ya estaba clickeando la fila; el refresh posterior al guardado
   re-aplicaba ese filtro y el producto editado desaparecía de la lista.
2. **`↻ Refresh` ignoraba el filtro activo** (le faltaba
   `hx-include="#product-filters"`, que sales/purchases ya tenían): la lista
   mostraba el catálogo completo mientras el control y la URL seguían
   filtrados. Ese es el "hago refresh y aparece".
3. **Save no cerraba el drawer** y el form conservaba un `hx-on::after-request`
   muerto: htmx limpia los listeners del form al desmontarlo en el swap, así que
   el cierre va por trigger. Se usa `HX-Trigger-After-Settle: product-saved`
   (el `HX-Trigger` común dispara antes del swap y el `htmx:afterSwap` que abre
   el drawer lo reabría).

Fixes: `hx-trigger="input delay:300ms, change"` en products/sales/purchases;
`hx-include="#product-filters"` en el Refresh de products; `product-saved`
(after-settle) + listener de body `closeProductDrawer`; el form del drawer queda
sin el handler muerto.

Verificación contra una copia de la base real (Yerba 500g + Agua 500ml) con el
binario actual: tipear "agua" filtra en vivo (1 fila, URL `?q=agua`); guardar
cierra el drawer (body vacío) y la fila sigue en la lista con el filtro
preservado; `↻ Refresh` con filtro mantiene 1 fila; borrar el filtro vuelve a 2
filas y a `/products`. Tests nuevos en `e2e/tests/test_products.py`
(live filter, refresh con filtro, fila visible tras guardar con filtro activo) y
contrato de headers en `src/routes/inventory_web.rs`.

## Desviaciones registradas
- `/products` deja de usar `partials/page_header.html`: el slot de una sola
  acción no puede alojar los dos botones de alta, así que usa el header de
  parties (link + pill + fila de título con botones). El test de page_header
  queda solo para el dashboard y se agrega
  `products_page_uses_modals_drawer_and_clickable_rows` con cobertura
  equivalente o mayor (modales, drawer, binding de fila, cards viejas ausentes).
- El select vacío del modal de producto se refresca con
  `/web/category-options?empty=No%20category`; el filtro sigue usando el label
  "All categories".
- `ProductsTemplate` pierde `low_stock` y `today` (quedaron sin uso al salir la
  card de movimiento): una query menos por carga de página.

## Follow-ups (fuera de alcance, pre-existentes)
- `UpdateSupplierRequest.phone/notes` y `UpdateCategoryRequest.parent_id`
  tienen el mismo patrón `Option<Option<T>>` sin `double_option`: hoy tampoco
  pueden limpiarse por JSON.
- La restauración por Back del filtro de `/products` no tiene test propio
  (el contrato `hx-history-elt` + sync de `base.html` está cubierto por el test
  de `/sales`).

## Next step
- Decidir rama/commit/PR encadenado (el diff es grande: ~2k líneas entre 11
  archivos). Nada commiteado todavía; hoy vive en el working tree de `main`.
