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
  solución no es mandarlo por header (`HX-Trigger-After-Settle`): el nombre del
  producto es UTF-8 arbitrario, y aunque `HeaderValue` **sí** acepta bytes ≥ 0x80
  (`is_valid`, http 1.5.0: `b >= 32 && b != 127 || b == b'\t'`), un header
  no-ASCII llega al cliente como mojibake porque XHR decodifica los bytes de
  header como ISO-8859-1; y re-escapar el nombre a ASCII a mano rompe con
  caracteres no-BMP (`serde_json` deja el UTF-8 crudo y `\uXXXX` necesita
  surrogates). Se resuelve con un guard de precedencia en el
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
- [x] T3 — Verificación completa (cargo test, check, e2e) + docs + cierre. → `1622bdc` + commit de corrección

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
  descartó el header payload por el mojibake del header no-ASCII (ver la
  corrección de cierre: `HeaderValue` no era el límite, lo es el decodificado
  del cliente).
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

## Cierre (T3)

### Verificación completa de la rama (`1622bdc`)
- Verificación independiente (read-only, otro agente) sobre el commit, árbol
  limpio: **PASS WITH FINDINGS**, rama apta para revisión. Los **5 criterios de
  aceptación cumplidos con evidencia re-derivada**, no heredada.
- Suites: `cargo test` **359 passed / 0 failed**; `cargo check --all-targets`
  **0 errores**, sin un solo warning atribuible a líneas tocadas;
  `scripts/e2e.sh` **completa**: 55 passed / 4 skipped / 0 failed (los 4 skipped
  son probes opt-in); `scripts/build-css.sh` reproducible.
- No-vacuidad re-derivada de cero con tres probes: filtrado neutralizado →
  **8 tests fallan**; chequeo de pertenencia sacado → fallan los 2 negativos;
  guard borrado → el test e2e **falla en navegador real** con
  `Actual value: Create product saved`. Todo revertido y probado por md5.
- Interacción T1×T2 probada en Chromium real por caminos: alta filtrada (el OOB
  **no** se filtra al swap principal, `#notice` tiene 1 box del servidor,
  `#low-stock-list` refresca coherente), alta sin filtro (aviso genérico, sin
  marcador), error a mitad (4xx, cero OOB en `#product-list`, un aviso de
  error), y POST de browser común (303 sin marcador en el body). Prueba a nivel
  bundle de que no puede filtrarse: `swapResponse` llama `handleOutOfBandSwaps`
  antes del swap principal y `oobSwap` remueve el elemento OOB de las dos ramas.
- Los 21 forms con `data-action` enumerados: el único emisor del marcador es el
  camino de alta filtrada, y la rama de error no tiene una línea cambiada.
- Sin restos: 0 referencias a `all_product_stocks`, sin TODO/FIXME nuevos, sin
  imports muertos, los 4 renders de `ProductListPartial` pasan por un filtro.

### Correcciones de exactitud (F1/F2/F3 de la verificación de cierre)
- **F1 (README, overstated)** — decía "every product mutation answer … the
  mutation forms carry `hx-include`": es falso para `POST /web/products/edit`,
  cuya rama no-drawer toma el filtro del query string. Corregido: la excepción
  queda nombrada y se aclara que ninguna página alcanza esa rama.
- **F2 (rationale falso, el más importante)** — sostuve que `HeaderValue` es
  ASCII-estricto y que un nombre con acento habría dado 500. **Es falso**: en
  `http 1.5.0`, `is_valid(b) = b >= 32 && b != 127 || b == b'\t'` acepta bytes
  ≥ 0x80, así que el header se construye sin problema. El modo de falla real es
  **mojibake**: XHR decodifica los bytes del header como ISO-8859-1. La decisión
  de transporte (body) **no cambia y sigue siendo la correcta** —evita el
  mojibake y el re-escape manual a ASCII, que rompe con no-BMP—, pero la razón
  escrita estaba mal. Corregido en `README.md`, en el comentario de
  `create_non_ascii_names_under_filter_stay_2xx_with_the_notice` y en este
  documento. El mensaje del commit `fed9bfc` conserva la afirmación vieja: se
  deja como está a propósito, porque el commit de corrección la nombra y
  reescribir historia invalidaría las hashes que la verificación usó como
  evidencia.
- **F3 (“byte for byte”, literalmente falso)** — los dos mirrors difieren en
  orden de atributos, whitespace, el marcador y el `<a>` del filtro. Corregido a
  "mismas clases, mismos `data-notice`/`role` y mismo botón de descarte", con
  los divergentes nombrados.
- **F4 (aceptado, dependiente de datos)** — el guard escanea el body entero: un
  producto llamado literalmente `data-notice-server` haría perder el aviso
  genérico de otra request. Sin crash ni dato incorrecto; direccionalmente
  seguro y documentado.
- **No verificado, sigue abierto** — sin cobertura e2e de nombres no-ASCII
  (solo Rust); ningún test afirma la lista de clases de los dos mirrors; el
  Back del filtro de `/products` se apoya en el test de `/sales` (mecanismo
  compartido, sin test propio).

### Carga de revisión
- Rama: 937+/29- en 9 archivos. Por categoría: código 167, templates 73, tests
  459, docs 267. Sin el documento de tarea, la superficie revisable son **291**
  líneas para T1 y **434** para T2.
- Corte en dos PRs posible **sin publicar un estado intermedio sin probar**: el
  verificador corrió el árbol de T1 (`bc53a08`) en un worktree aislado →
  `cargo test` 354 passed, e2e completa 54 passed / 4 skipped. PR1 = T1 (bajo el
  umbral de ~400), PR2 = T2 (apenas por encima). Decisión del usuario.

## Entrega (2026-09-18)
- Dos PRs **apilados**, ambos mergeados a `main`:
  - PR #38 (`fix/filter-honest-mutation-answers`, `type:bug`) → merge `7e4c55b`.
    Slice T1: los cuatro handlers de mutación filter-honest.
  - PR #39 (`fix/filter-honest-product-mutations`, `type:feature`) → merge
    `49d5fa2`. Slice T2: el aviso create-bajo-filtro. Cerró la issue #37, que
    pasó a CLOSED/COMPLETED.
  - #38 dice "Part of #37" y #39 lo cierra: el criterio de aceptación del
    create-bajo-filtro recién se cumple en T2, así que cerrarlo en #38 habría
    sido mentira de bookkeeping.
- Slicing: 937+/29- totales. Sin el doc de tarea, la superficie revisable era
  **291** líneas (T1) y **461** (T2). Una sola pasada honesta de corte; T2 queda
  sobre el budget de 400 y por eso lleva `size:exception`, aceptado por el
  usuario al mergear. No hay corte mejor: el aviso, el guard y el test de
  navegador son un solo comportamiento, y separar el test de lo que verifica
  publicaría un estado intermedio sin probar.
- Gate post-merge (verificación independiente sobre `main` ya mergeado):
  `main^{tree} == e333b42^{tree}` (`f204282c…`), o sea **los dos merges no
  perdieron ni inventaron un hunk**; `cargo test` 359 passed; `cargo check
  --all-targets` 0 errores y 51 warnings (el baseline exacto, ninguno sobre una
  línea tocada); `scripts/e2e.sh` completo 55 passed / 4 skipped;
  `scripts/build-css.sh` reproducible. Entre `6b5f025` y `main` entraron
  exactamente los 9 archivos del cambio, ninguno de más.
- **Error de proceso, registrado porque es reutilizable**: `gh pr merge 38
  --delete-branch` **cerró** #39 en vez de retargetearlo — GitHub cierra un PR
  cuando desaparece su base y no lo retargetea sólo. Recuperado recreando la rama
  padre en `ba3999e`, reabriendo #39, retargeteando a `main` y confirmando que el
  diff siguiera siendo el slice de T2 (7 archivos, +621/-26) antes de mergear.
  Regla para la próxima: en un stack, la rama padre no se borra hasta que mergea
  el hijo.
- Ramas locales y remotas borradas; `main` local y remoto en el mismo commit
  (`49d5fa2`), árbol limpio, una sola rama en ambos.

## Next step
- Nada pendiente de esta feature: #37 cerrada y los dos PRs mergeados con el
  gate post-merge en verde.
- Follow-ups abiertos, fuera de alcance (detalle en las secciones de arriba):
  asimetría de `web_edit_product` (toma el filtro del query string, sin caller
  in-repo), el wiring guard que no cubre el fragmento del drawer de producto, y
  los `Option<Option<T>>` sin `double_option` de suppliers/categories.
- Preexistente y ajeno a esta feature: 79 sesiones rancias "activas" de `roya`
  en Engram (por eso fallan cerrado los writes sin session id explícito) y el
  error `sync_target_closed_space` (43 mutaciones sin ack).
