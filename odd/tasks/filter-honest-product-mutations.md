# Filter-honest answers for the remaining product mutations (issue #37)

## Objective
Las cuatro respuestas de mutación que todavía ignoran el filtro activo pasan a
respetarlo, y el alta de producto bajo filtro deja de ser muda: cuando el
producto recién creado no matchea el filtro activo, el operador recibe un aviso
que lo nombra y le dice por qué no lo ve, con salida de un click.

## Problem
`web_create_product`, `web_create_movement`, `web_record_product_cost` y
`web_set_preferred_cost` tienen su rama no-drawer devolviendo
`all_product_stocks()` — el catálogo completo — mientras el caller mira una lista
filtrada. Es la misma trampa latente que #33 cerró en dos respuestas: hoy queda
enmascarada por los triggers de refresh (`product-created`, `movement-created`,
`product-cost-recorded`), que vuelven a pedir la lista con el filtro y ganan la
carrera. Un trigger caído o reordenado y la lista queda en desacuerdo con sus
propios controles de filtro. Encima hay un render redundante por acción.

El caso create carga además una decisión de producto que los otros tres no
tienen: con un filtro activo, la lista correctamente filtrada **no muestra** el
producto recién creado, y el operador puede leer eso como un alta fallida.

## Why
Una respuesta que dice "acá está tu lista" y devuelve otra cosa que la lista que
el operador está mirando es un defecto, no un detalle. La honestidad del filtro
es contrato del endpoint. Y el caso create, que sí es visible al usuario, se
resuelve sin mentir: se mantiene el filtro (que es lo correcto) y se le dice al
operador qué pasó.

## Decisión de producto (tomada por el usuario, 2026-09-18)
Opción A de cuatro evaluadas: **avisar y mantener el filtro**. La lista sigue
filtrada, y el aviso nombra el producto y explica la causa, con un botón
`Clear filter`. Descartadas: limpiar el filtro al crear (pierde contexto en
silencio), fijar la fila nueva arriba (swap OOB + estado no persistente), y
mantener el filtro sin avisar (no resuelve la queja de fondo).

## Scope
- `src/routes/inventory_web.rs`: los cuatro handlers (`create_product`,
  `create_movement`, `record_product_cost`, `set_preferred_cost`), los cuatro
  structs de form, y `all_product_stocks` si queda sin uso.
- `templates/products.html`: `hx-include="#product-filters"` en el modal de
  alta, rename del select de categoría propia, listener del aviso.
- `templates/partials/product_detail.html`: `hx-include` en los forms de
  movimiento, costo y preferido.
- `templates/base.html`: guard de precedencia en el handler genérico
  `htmx:afterRequest`, comentario cruzado con el box del servidor.
- `templates/partials/notice.html` (nuevo): el box, en `templates/` para que
  Tailwind lo escanee.
- `src/smoke_tests.rs`: el helper `create_product_full_via_web` por el rename.
- `e2e/tests/test_products.py`: el flujo real en navegador.
- Fuera de alcance: `web_edit_product` (cerrado en #33), los follow-ups
  `Option<Option<T>>` de suppliers/categories, y N6 de `redesign-interface`.

## Constraints
- Mecanismo único: `hx-include="#product-filters"` — el que ya usan los tres
  forms de ciclo de vida y el que documenta `ProductIdForm`. No se introduce JS
  nuevo de sincronización ni un form espejo: el valor del filtro viaja leído del
  DOM en el momento del request, nunca renderizado por el servidor (eso sería
  stale, porque el filtro cambia por htmx sin recargar la página).
- El choque de clave es real y hay que nombrarlo: el modal de alta ya manda
  `category_id` para la categoría **propia** del producto, así que el
  `category_id` del filtro no puede viajar en el mismo body. Se resuelve
  renombrando el campo propio del modal a `product_category_id`. El form del
  drawer de edición **no** lleva `hx-include`, así que conserva su `category_id`
  y la asimetría queda documentada donde se lee.
- El aviso se deriva en el servidor re-ejecutando `filter_products` con el
  filtro activo y buscando el id nuevo: la pertenencia la decide el servicio
  (que es la autoridad sobre el matching de nombre/SKU/barcode), no una
  comparación paralela escrita a mano ni el cliente.
- El aviso tiene que pisar el genérico "Create product saved" que ya emite
  `base.html` en `htmx:afterRequest`. **Premisa inicial refutada con evidencia**
  (ver T2): htmx 1.9.12 corre la fase de swap (`beforeSwap` → `afterSwap`)
  **antes** de `htmx:afterRequest`, así que un aviso renderizado en el body se
  swappea primero y el genérico lo pisa después — al revés de lo supuesto. La
  solución no es mandarlo por header (`HX-Trigger-After-Settle`), porque el
  nombre del producto es UTF-8 arbitrario y `HeaderValue` es ASCII-estricto: un
  producto "Yerba Ñandú" convertiría un alta exitosa en 500, y escaparlo a mano
  rompe con caracteres no-BMP. Se resuelve con un guard de precedencia en el
  handler genérico existente: saltea el aviso genérico cuando el body de la
  respuesta trae el box (`data-notice-server`). Es independiente del orden —la
  decisión sale del cuerpo de la respuesta, no del DOM post-swap— y falla seguro
  en las dos direcciones. El `HX-Trigger: product-created` actual no se toca.
- El markup del aviso vive en `templates/partials/notice.html`, no en un string
  de Rust: Tailwind v4 escanea **solo** `templates/` (`@source "../templates"`
  en `assets/tailwind.css`), así que una clase agregada a markup armado en Rust
  renderiza sin estilo con todos los tests en verde. El precedente de
  `web_category_options` no cubre el caso: arma `<option>` **sin clases**.
- Los cuatro tests nuevos tienen que ser no-vacuosos: fallan si se saca el
  filtrado, mismo estándar que #33.
- Tailwind: nada de palabras sueltas que sean utilidades (`blur`, `inline`) en
  la prosa de los templates.

## Authorized scope
Los cuatro handlers + sus structs de form + los tres templates + el helper de
smoke + el test e2e. Rama `fix/filter-honest-product-mutations`. Commit por
unidad de trabajo; push y PR son decisión del usuario.

## Acceptance criteria
- [ ] Ninguna respuesta de mutación renderiza una lista que ignora el filtro
      que su caller está mirando
- [ ] El comportamiento create-bajo-filtro está decidido (opción A) y pineado
      por test
- [ ] Los tests fallan si se saca el filtrado (no-vacuosos)
- [ ] `cargo test` full en verde, `cargo check --all-targets` limpio
- [ ] `scripts/e2e.sh -k products` verde, y el flujo create-bajo-filtro cubierto
      en navegador real

## Applicable checks
- `cargo test` (full), `cargo check --all-targets`
- `scripts/e2e.sh -k products` (y la suite completa antes de cerrar)
- No-vacuidad probada rompiendo el filtrado a propósito

## Tasks
- [x] T1 — Los cuatro handlers filter-honest: structs + ramas no-drawer por
      `filter_products`, rename del campo propio del modal, `hx-include` en los
      forms, helper de smoke. Con sus tests Rust. → `bc53a08`
- [x] T2 — Aviso create-bajo-filtro: derivación server-side, transporte por
      body (swap OOB hacia `#notice`), guard de precedencia en `base.html`,
      markup en `templates/`. Con tests Rust y e2e. → `fed9bfc`
- [ ] T3 — Verificación completa (cargo test, check, e2e) + docs + cierre.

## Progress
- 2026-09-18: documento creado, rama `fix/filter-honest-product-mutations`
  (desde `main` limpio en `6b5f025`). Baseline verificado: `cargo test` 350
  passed, árbol limpio, sincronizado con `origin/main`.
- 2026-09-18: T1 completo en `bc53a08`. `all_product_stocks` reemplazado por
  `filtered_list_html(state, q, category_id)`; los cuatro handlers leen el
  filtro del body; el campo propio del modal pasó a `product_category_id`.
- 2026-09-18: T2 completo en `fed9bfc`. El aviso viaja en el body como swap OOB
  hacia `#notice`; el guard de precedencia descarta el aviso genérico cuando el
  servidor ya renderizó el suyo. La premisa de orden del diseño inicial
  (`HX-Trigger-After-Settle`) quedó refutada por medición y se reemplazó.

## Verification evidence

### T1 (`bc53a08`)
- Writer: `cargo test` 354 passed / 0 failed (baseline 350, +4 nuevos);
  `cargo check --all-targets` 0 errores, 51 warnings, ninguno atribuible a las
  líneas nuevas.
- Verificación independiente (read-only, otro agente): **PASS WITH FINDINGS**,
  ningún hallazgo bloqueante ni causado por el cambio. Re-derivó la
  no-vacuidad por su cuenta con **dos** probes: rompiendo el filtrado
  (`filter_products("", None)`) los 4 tests fallan por la aserción esperada, y
  con un filtro que no matchea nada (`"__matches_nothing__"`) los 4 vuelven a
  fallar por la aserción opuesta. O sea las aserciones atan en las dos
  direcciones y ningún test pasa sobre una lista vacía. Restauración probada
  por `md5sum` del archivo y del diff completo (idéntico antes y después).
- Inventario repo-wide de posters a los cuatro endpoints (incluido `e2e/`,
  `README.md`, `openspec/`): sólo el modal, los tres forms del drawer y el
  helper de smoke tocaban el endpoint de alta; el rename no rompe ninguno. Los
  `curl` del README apuntan a `/api/*`, no al form web. Los e2e llenan el modal
  por `name="sku"/"name"/"sale_price"` y eligen la categoría por **id**
  (`#new-product-category`), así que el rename no los toca.
- El choque de clave se comprobó decodificando `static/htmx.min.js` (1.9.12):
  en `getInputValues` los campos del form que dispara se copian **sobre** los
  del `hx-include` (`n = le(n, i)`), así que un nombre repetido hace ganar al
  form propio y **descarta en silencio** el valor incluido. Con el nombre viejo
  el filtro habría perdido su `category_id` y sólo habría viajado `q`. Esto es
  lo que convierte el rename en arreglo semántico, no en cosmética.
- Limpieza confirmada: `grep all_product_stocks` → 0 matches; los cuatro
  renders de `ProductListPartial` existentes pasan por un filtro
  (`filtered_list_html`, `web_product_list`, `web_edit_product`,
  `product_lifecycle_response`).
- Sin caminos stale: `hx-include` lee el DOM en el momento del request, y
  `base.html` re-sincroniza los filter forms contra `location.search` en
  `htmx:historyRestore`, así que el render inicial y el Back coinciden.
- No verificable en T1 (fuera de autorización): comportamiento en navegador
  real — lo cubre T2/T3 con la suite e2e.

### T2 (`fed9bfc`)
- Writer: `cargo test` 359 passed / 0 failed (354 antes de T2, +5);
  `cargo check --all-targets` 0 errores, 51 warnings pre-existentes;
  `scripts/e2e.sh -k products` 13 passed / 1 skipped;
  `scripts/build-css.sh` reproducible (`static/tailwind.css` md5
  `e562f79b00c7906faec139cf33d40132` sin cambios).
- **Premisa del diseño refutada, con evidencia, antes de escribir el código
  equivocado.** El brief afirmaba que el swap OOB landa después de
  `htmx:afterRequest`. El writer decodificó `static/htmx.min.js` y midió en
  Chromium real con Playwright: `["beforeSwap","afterSwap",
  "afterRequest:successful=true","notice-js"]` — el swap va **primero**
  (`b.onload` ejecuta `M(n,I)` y recién después `ce(n,"htmx:afterRequest",I)`),
  así que el aviso genérico "Create product saved" pisaba el box del servidor.
  Implementado tal cual el brief, el test e2e habría fallado. Se aprobó la
  opción A (guard de precedencia con marcador `data-notice-server`) y se
  descartó el header payload por el límite ASCII de `HeaderValue`.
- Verificación independiente (read-only, otro agente): **PASS WITH FINDINGS**,
  sin defecto de correctitud. Re-derivó la no-vacuidad con **cuatro** probes:
  con `hidden` forzado a `false` fallan 3 tests (y el negativo queda verde, que
  es lo correcto); con el chequeo de pertenencia sacado fallan los dos tests
  negativos, o sea las aserciones atan en las dos direcciones; con el guard
  borrado **el test e2e falla en navegador real** con
  `Actual value: Create product saved`; restauración probada por md5 de los
  cuatro archivos y por `git diff --stat` idéntico.
- Verificado que el aviso se ve: los **30 tokens de clase** del markup existen
  en el `static/tailwind.css` commiteado (parseo estricto de selectores, no
  substring). Sin esto un aviso sin estilo pasaría todos los tests, que afirman
  texto y estructura, no apariencia.
- Verificado el «radio de explosión» del guard: 21 forms con `data-action`
  enumerados; el único emisor del marcador es `web_create_product`. La rama de
  error (`htmx:responseError`) no tiene una sola línea cambiada, y el guard
  corre después de `if (!evt.detail.successful) return`, así que nunca se aplica
  a un error.
- Verificado que el sniff de substring es independiente del orden: la decisión
  sale de `evt.detail.xhr.responseText`, nunca del DOM post-swap. Si una versión
  futura de htmx invirtiera las fases, el guard seguiría sólo suprimiendo y el
  swap OOB dejaría el box igual.
- Verificado que el test e2e es sensible a los cuatro modos de falla relevantes
  (guard borrado, marcador ausente, `hidden` forzado, transporte por header):
  ninguno escapa.
- Fuerza del `HX-Trigger: product-created` y del redirect no-HTMX: byte-idénticos
  a `HEAD`.

### T2 — hallazgos de la verificación
- **F1 (cerrado en T2)** — Tailwind v4 escanea solo `templates/`, así que las
  clases del aviso armadas en Rust nunca se escanean: hoy están en el CSS de
  prestado. Latente (un aviso sin estilo con todos los tests verdes) y lo
  introducía el diseño. Cerrado moviendo el markup a
  `templates/partials/notice.html`.
- **F2 (cerrado en T2)** — el propio documento de tarea describía el diseño
  viejo (`notice()` con link de acción, `HX-Trigger-After-Settle`). Corregido.
- **F3 (aceptado, direccionalmente seguro)** — el guard usa
  `responseText.includes('data-notice-server')` sobre el body entero: un
  producto o categoría llamado literalmente así saltearía el aviso genérico de
  esa request. Sin crash ni dato incorrecto, y el único emisor del marcador es
  un handler. Se acepta con el contrato escrito en el comentario.
- **F4 (aceptado)** — `render_product_list(&[ProductStock])` clona con
  `to_vec()`; ambos call sites tienen la `Vec` propia y podrían moverla. El
  verificador lo juzgó no valioso (app local, catálogo chico, un clone por
  request). Se deja.
- **F5 (informativo)** — `body_filter_is_active` re-deriva la regla lenient de
  `WebProductFilter::parsed()`. Verificado token-consistente; queda como
  defensa.
- **No verificado** — el escapado adversarial ahora sí tiene test
  (`create_notice_escapes_html_specials_in_the_product_name`), pero **no hay
  cobertura e2e de nombres no-ASCII** (el test es a nivel Rust), y **ningún test
  afirma la lista de clases**: los dos mirrors (template Askama y string JS en
  `base.html`) siguen siendo un contrato textual con comentario cruzado en las
  dos puntas.

### Hallazgos del verificador de T1, diferidos a follow-up (pre-existentes, fuera de alcance)
- **F1** — `web_edit_product` (rama no-drawer) sigue tomando el filtro del
  query string, no del body: es el arreglo de #33 y hoy no tiene caller
  in-repo. La asimetría con el body queda documentada en el código. Pre-existente.
- **F2** — El wiring guard (`guarded_pages()` en `src/smoke_tests.rs`) no cubre
  el fragmento del drawer de producto (`/web/products/detail/{id}`), así que los
  `hx-include` nuevos de `product_detail.html` no están validados por
  `seeded_pages_render_only_wired_htmx_targets`. El selector es correcto
  (`#product-filters` existe en la página que aloja el drawer) y el hueco ya
  existía para los forms de ciclo de vida de #33.

## Next step
- T1: delegar el writer con las superficies de edición exactas.
