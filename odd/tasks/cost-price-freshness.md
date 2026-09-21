# F2 — Frescura de `cost_price`: aviso en el draft + badge permanente

Feature hermana de `odd/tasks/product-markup-pricing.md` (F1). F1 deriva el precio
de venta de `cost_price`; esta feature se ocupa de que `cost_price` **no esté
viejo**. Son independientes: F1 funciona igual sin esto, solo que derivando de un
costo que puede ser antiguo.

## Objective
Que `products.cost_price` refleje el costo real del proveedor preferido, con
aprobación humana explícita y **dos detectores complementarios**: un aviso en el
momento de cargar la compra, y un badge permanente en el producto para cuando ese
momento pasó sin que nadie mirara.

## Problem
`products.cost_price` **nadie lo mantiene**. Las compras nunca lo escriben: en la
confirmación actualizan el satélite `product_supplier_costs`, y el propio código
lo declara:

- `openspec/specs/purchases/spec.md:46` — *"**A purchase never writes
  `products.cost_price`.** That column is the fallback for products with no
  supplier row, such as services."*
- `src/services/suppliers.rs:9-11` — *"`products.cost_price` is never written
  here: the satellite wins when the product has rows, and callers fall back to the
  column only when it does not."*
- test `src/services/purchases.rs:2123` —
  `ac10_purchase_never_writes_cost_price_and_satellite_wins`, con
  `assert_eq!(stored.cost_price, dec("5"), "cost_price must stay untouched")`.

Resultado: la columna solo cambia si alguien edita el producto a mano, y en la
práctica queda congelada en el valor del alta.

## Why
El costo de compra es un dato que el sistema ya conoce — vive en el satélite y se
actualiza en cada compra confirmada — pero no llega al producto. Con F1 encima,
eso significa derivar precios de venta de un costo viejo, y el drift es silencioso.

## Decisiones del usuario
1. **Solo el botón escribe `cost_price`.** El flujo de compra sigue sin escribir
   la columna **nunca**: lo que escribe es una acción web explícita que dispara el
   humano. Consecuencia: **la invariante AC10 sobrevive intacta** — spec de
   purchases y su test no se tocan. Se evaluó la sincronización automática en
   `record_cost` y se descartó justamente por eso.
2. **Cascada**: cuando `cost_price` cambia, si el producto tiene markup cargado el
   precio de venta **se recalcula solo** (decisión tomada en F1). El humano
   aprueba el costo, el precio sigue de forma determinista.
3. **Dos superficies, no una.** El aviso del draft es efímero; el badge es
   permanente. Como el botón es el único escritor, el aviso es el único detector
   *en el momento*, y sin el badge un operador que confirma sin mirar deja el
   costo viejo para siempre sin que nada lo señale.
4. **Interpretación confirmada**: el aviso compara **el costo de la línea que se
   está cargando contra `products.cost_price`** del producto — el costo que el
   producto tiene guardado hoy. Es la comparación que mantiene la columna fresca y
   la que da sentido al botón "actualizar el costo". La otra lectura (que el
   *proveedor* subió su precio respecto de su propio histórico) **ya está
   cubierta** por `price_alert`/`PriceAlert::Raised` y no necesita nada nuevo.
5. **Permiso del botón: `InventoryWrite`, con el botón siempre visible.** El botón
   escribe un *producto*, así que se gatea como toda escritura de producto. Quien
   solo tiene `purchases.create` ve el aviso y el botón, y al clickear recibe la
   página de prohibido — visible, no un fallo silencioso. Es el patrón dominante
   del repo (el markup es cortesía, el handler es la enforcement) y evita plomear
   los permisos del principal hasta el template. Se descartó gatear con
   `PurchasesCreate`: le daría a un comprador la escritura de productos, que es
   escalada de privilegio (invariante 12 — el `any-of` es para pantallas de
   lectura, "un índice de lo que el principal ya puede leer y nunca un grant
   nuevo").
6. **Secuencia de rama: cerrar F1 primero.** Se mergea `feat/product-markup-pricing`
   y F2 ramifica desde `main`. Reviews chicos e independientes. Riesgo aceptado: si
   F1 cambia en el review, F2 se ajusta (el botón depende de su derivación).

## Contexto de arquitectura: draft y documento son la misma fila

Esto define dónde vive cada detector, así que queda registrado.

`purchases` es **una tabla con tres estados**, no dos documentos distintos
(`migrations/20240101000015_create_purchases.sql`):

```sql
status TEXT NOT NULL CHECK (status IN ('Draft', 'Confirmed', 'Cancelled')),
purchase_number TEXT NULL UNIQUE,   -- NULL sólo mientras es Draft
```

No existe tabla de drafts/quotes/orders en las 35 migraciones. El draft **es** la
compra antes de tener número, y el número se asigna **al confirmar, nunca en
draft** (invariante 8; test `src/services/purchases.rs:1441`, *"Draft must not
consume a document number"*). Eso da cuatro estados semánticos con tres etiquetas:

| Estado real | `status` | `purchase_number` |
|---|---|---|
| Borrador | `Draft` | `NULL` |
| Documento | `Confirmed` | `YYYY-PURCH-NNNNNN` |
| Cancelado (era documento) | `Cancelled` | conserva el número (`:2369`) |
| Cancelado (nunca fue documento) | `Cancelled` | `NULL` (`:2345`) |

Reglas que lo sostienen y que condicionan esta feature:

- Las líneas mueren con el padre (`purchase_lines.purchase_id ... ON DELETE CASCADE`).
- **Solo el draft se puede borrar**: `delete_draft`
  (`src/repositories/purchase_repo.rs:537`) devuelve `false` sin tocar nada si la
  compra está `Confirmed` o `Cancelled` (tests `:1453`, `:1495`). Un documento real
  nunca se borra, se cancela.
- **Las mutaciones de línea exigen Draft** (`ensure_draft`,
  `src/services/purchases.rs:176`), y confirmar dos veces se rechaza (`:562`).
- El satélite `product_supplier_costs` se actualiza **al confirmar**.

**Consecuencia buena**: el aviso solo puede aparecer en la fase Draft, y esa
ventana coincide exactamente con la ventana en que el documento todavía no es real
— nada que se avise ahí tiene consecuencias sobre un documento.

**Consecuencia mala**: por lo mismo, el aviso es efímero. De ahí el badge.

## Lo que ya existe (no reinventar)
- **La lectura del costo de referencia**: `SupplierService::reference_cost`
  (`src/services/suppliers.rs:233-245`) devuelve "proveedor preferido, si no el más
  barato, si no `None`". Es exactamente lo que alimenta el badge.
- **La detección de suba del proveedor**: `price_alert()`
  (`src/services/suppliers.rs:248-257`) con `PriceAlert::Raised`, derivado de
  `previous_cost` vs `current_cost`, usado por los drawers de producto y proveedor.
- **El drawer del producto** (`templates/partials/product_detail.html`), que ya
  muestra los costos por proveedor y es donde va el badge.
- **El guard de draft** (`ensure_draft`) y el punto donde nace el costo de línea
  (`add_line`, con el fallback `None => product.cost_price` en
  `src/services/purchases.rs:320`).

## Scope
**T0 ejecutado**: el módulo de compras ya está mapeado. Resultado clave: **ninguno
 de los dos servicios necesita cambios** — `update_product` ya re-deriva el precio
(de hecho su doc comment ya anticipa esta feature) y `reference_cost` ya existe.

**Señal A (aviso efímero)** — el plumbing es mínimo:
- `PurchaseLineView` (`src/models.rs:1076-1095`) **no** lleva el `cost_price` del
  producto, pero el loop que lo construye (`PurchasesService::record_from_detail`,
  `src/services/purchases.rs:419-448`, el fetch en `:428`) **ya fetchea el producto
  entero**. Es un campo y una asignación, sin query nueva.
- Como el aviso se renderiza **por fila**, cubre agregar y editar línea sin trabajo
  extra. Pero ojo: **editar línea no tiene UI** — `web_update_line`
  (`src/routes/purchases_web.rs:692`) está ruteado y testeado, pero
  `purchase_detail.html` solo renderiza el botón de borrar. En la práctica el aviso
  solo *aparece* al agregar.
- Tiene que vivir **dentro** de `record_money`
  (`templates/partials/purchase_detail.html`, loop de filas en `:33-49`): agregar
  línea swapea solo `#purchase-record-money` (el picker hace `hx-select`), así que
  un aviso fuera de ahí no aparece hasta un swap completo de `#purchase-record`.
- La acción de aplicar es un handler nuevo en `purchases_web.rs` que llama
  `inventory_service.update_product` con un patch de solo `cost_price`.

**Señal B (badge permanente):** `reference_cost` (`src/services/suppliers.rs:236-247`)
 ya devuelve "preferido, si no el más barato, si no `None`", pero **ninguna ruta lo
 expone hoy** (`grep reference_cost src/` da solo el servicio y sus tests). Necesita
 una llamada en `product_detail_html` (`src/routes/inventory_web.rs:460-498`) y un
 campo nuevo en `ProductDetailPartial` (`:126-142`).

Ubicación decidida: el badge va en la **Card B (supplier costs)**
 (`templates/partials/product_detail.html:179-243`), no al lado del precio, porque
 ahí nacen los costos de proveedor y el texto de estado vacío **ya** explica el
 fallback al `cost_price` del producto (`:184-185`). El badge hereda el doble gate
 del drawer (`inventory.read` **y** `purchases.costs.read`,
 `src/routes/inventory_web.rs:445-451`), así que solo lo ve quien tiene los dos.

Datos de contexto confirmados en el mapeo: la comparación de la Señal A sale del
 producto ya fetcheado en `add_line` (`src/services/purchases.rs:307`, con el
 fallback en `:318-320`), y el `ensure_draft` (`:176`) garantiza que el aviso solo
 puede existir en fase Draft.

Fuera de alcance: la spec de purchases (no cambia), y el formato de dinero de los
costos por proveedor (diferido desde F1).

## Constraints
- `products.cost_price` es `NOT NULL DEFAULT '0'`: "sin costo" se manifiesta como
  `0`, no como NULL. El aviso y el badge deben tratar el `0` como "no hay costo
  todavía", no como un costo real que se puede comparar.
- Si el producto tiene markup y no tiene costo, F1 rechaza la derivación. El botón
  de acá es el camino que saca al producto de ese estado.
- `reference_cost` devuelve `None` cuando el producto no tiene filas de proveedor;
  sin costo de referencia no hay badge.
- strict_tdd: true (`openspec/config.yaml`); sin CI, las suites corren local.
- **Un e2e es frágil ante cambios cosméticos**: `e2e/tests/test_picker.py:214`
  aserta el texto de la fila de compra con un regex (`HARNESS-SPARE\s+4\s+\$`).
  Meter un badge entre la celda del producto y la de cantidad cambia el orden del
  texto y lo rompe: hay que ajustarlo a propósito, no "arreglar" el test a ciegas.
- **El guard genérico de wiring va a probar la ruta nueva**
  (`src/smoke_tests.rs:1412-1501`): el endpoint del botón tiene que estar
  registrado y responder, o el guard falla. Es a favor, la ruta queda vigilada.
- `e2e/tests/test_purchases.py` **no existe**: la cobertura de compras en
  navegador vive en `e2e/tests/test_picker.py:183` y `e2e/tests/test_parties.py:340`,
  con helpers en `e2e/helpers.py` (`create_purchase_draft` `:529`,
  `add_purchase_line` `:551`, `confirm_purchase` `:574`).

## Acceptance criteria
- [ ] Al cargar una línea de compra Draft con costo **mayor** que
      `products.cost_price`, aparece el aviso con el costo actual y el nuevo.
- [ ] El aviso **no** aparece si el costo es igual o menor, ni si el producto no
      tiene costo guardado (`0`).
- [ ] El botón actualiza `cost_price` y, si el producto tiene markup, el
      `sale_price` derivado queda recalculado.
- [ ] La compra **nunca** escribe `products.cost_price` por sí sola: el test AC10
      sigue verde sin modificaciones, y la spec de purchases no cambia.
- [ ] En el drawer del producto, cuando `reference_cost` difiere de
      `products.cost_price`, aparece el badge con los dos valores; no aparece
      cuando coinciden ni cuando no hay costo de referencia.
- [ ] Con `cost_price = 0` el badge trata el caso como "sin costo", no como una
      diferencia contra cero.
- [ ] Ambas señales son **derivadas**: nada nuevo se almacena.
- [ ] `cargo test` verde y `cargo check --all-targets` sin errores.
- [ ] `scripts/e2e.sh -k purchases` y `-k products` verdes.

## Applicable checks
- `cargo test`, `cargo check --all-targets`
- `scripts/e2e.sh -k purchases`, `scripts/e2e.sh -k products`
- `scripts/build-css.sh` si cambian clases en los templates

## Tasks
- [x] T0 — Scout read-only del módulo de compras. **Ejecutado**: el conjunto más
      chico de archivos es seis — `src/models.rs` (campo en `PurchaseLineView`),
      `src/services/purchases.rs` (poblarlo en `record_from_detail`),
      `templates/partials/purchase_detail.html` (aviso en `record_money`),
      `src/routes/purchases_web.rs` (handler de aplicar + ruta),
      `src/routes/inventory_web.rs` (`reference_cost` en `product_detail_html` +
      campo en `ProductDetailPartial`) y `templates/partials/product_detail.html`
      (badge). Confirmado: **no** hay que tocar `src/services/inventory.rs` ni
      `src/services/suppliers.rs`.
- [x] T1 — **Señal B, derivación y badge.** → `c123678`. La condición quedó en
      Rust con las tres compuertas (hay referencia, el costo guardado no es 0, y
      los dos difieren), el badge en la Card B, y ningún token de clase nuevo. Cinco
      tests de ruta, incluido el que discrimina: con un proveedor preferido más
      caro y uno no preferido más barato, gana el preferido.
- [x] T2 — **Señal B, e2e.** → `test(inventory): cover the stale cost badge from the
      browser`. Cuatro tests de navegador: difiere (afirma el nodo exacto
      `reference $12.50 • stored $5.00`), coincide, sin filas de proveedor, y
      `cost_price = 0` con costo de proveedor presente. La ausencia se afirma
      contando el label exacto `stale cost`, que se verificó que no aparece en
      ningún otro lado del árbol. Validado revirtiendo el fix: con el badge
      incondicional fallan los tres tests de ausencia (el de presencia sigue
      pasando, y eso es correcto: mostraría los mismos valores dinámicos).
      `scripts/e2e.sh -k products`: 22 passed, 1 skipped (el probe opt-in).
      `cargo test`: 782, sin moverse.
- [x] T3 — **Señal A, modelo y servicio.** → `d55ed00`. La comparación en
      `PurchaseLineView` poblada en `record_from_detail`, sin query nueva (el
      producto ya se fetcheaba para nombre y SKU). Cinco tests de servicio, y la
      verificación confirmó que todos corren por el constructor real y no
      construyen la vista a mano. `cargo test` 787.
- [x] T4 — **Señal A, el aviso.** → `1ac6555`. Render como **fila propia debajo**
      de la fila de la línea, dentro de `record_money`, solo en Draft con la misma
      compuerta que el botón de borrar. `colspan` 5, que es el número de celdas de
      cabecera que el Draft renderiza. `cargo test` 790, e2e `-k picker` 8 passed.
      **El regex de `test_picker.py:214` NO se tocó, y fue la decisión correcta**:
      asume adyacencia entre producto y cantidad, así que el aviso va después del
      subtotal y el assert sigue siendo real. Metido entre esas celdas lo habría
      roto por una razón de layout. Verificado con hash de blob: idéntico en
      worktree, `d55ed00` y HEAD.
- [x] T5 — **Señal A, el botón.** → `844248c`. Ruta
      `POST /web/purchases/{purchase_id}/lines/{line_id}/apply-cost`, gateada
      `Require<InventoryWrite>`, que escribe vía `update_product` con un patch de
      solo `cost_price`. `cargo test` 797. El handler **no toma ningún extractor de
      body**: el producto y el costo salen de la línea guardada, resuelta por
      `get_detail(purchase_id)` — que es también lo que impide aplicar el id de
      línea de otra compra. La verificación confirmó las dos cosas y el test de la
      frontera se validó haciendo el lookup global y viéndolo fallar 200 contra 404.
      El comentario del test de T4 que sobreafirmaba el scopeo quedó ajustado en la
      misma slice (ahora afirma el aviso dentro de `#purchase-record-money`).
- [ ] T6 — **Señal A, e2e del flujo.** Aviso → botón → `cost_price` actualizado →
      `sale_price` recalculado cuando hay markup.
- [ ] T7 — **Spec.** Change folder OpenSpec + promoción de `inventory`. La spec de
      **purchases no cambia** (AC10 intacto).
- [ ] T8 — **Verificación final independiente**: `cargo test`,
      `cargo check --all-targets`, `scripts/e2e.sh -k purchases` y `-k products`.
      La verificación de F1 encontró un bug alcanzable que las verificaciones por
      slice no vieron, así que esta no se saltea.

## Progress
- **Convención de entrega (decidido 2026-09-21)**: F1 entró a `main` con un push
  directo. El remoto lo reportó como `Bypassed rule violations`. Diagnóstico
  correcto, leído de la API (`gh api repos/ematiasm/roya/rulesets`): **el ruleset
  aplica a `~ALL`, no a la rama default** — o sea que cada push directo a
  *cualquier* rama viola la regla `pull_request`, y el rol Admin la bypassea
  siempre (`bypass_mode: always`). Con esa configuración la regla no gatea nada
  para el admin: solo avisa y deja pasar. También incluye `non_fast_forward` y
  `deletion`.
  Decisiones del usuario: **`main` queda como está, sin reescribir historia**, el
  **ruleset no se toca**, y de acá en adelante todo entra por PR. Consecuencia
  concreta para F2: la rama `feat/cost-price-freshness` va a `origin` y **se
  mergea por PR, nunca con un push directo a `main`**.
- Documento creado con las decisiones 1 y 2 (botón único escritor, cascada
  automática). Sin código.
- Se resolvió la pregunta de arquitectura previa: draft y documento **no** son
  documentos distintos, son estados de la misma fila. Eso confirmó que la ventana
  del aviso es la fase Draft (donde nada es todavía un documento) y expuso que el
  aviso es efímero, lo que llevó a la decisión 3 (badge permanente).
- Decisiones 3 y 4 registradas. T0 sin ejecutar: el detalle de T1+ espera el mapa
  del módulo de compras.
- **T0 ejecutado.** Mapa obtenido; el conjunto más chico de archivos son seis y
  ninguno de los dos servicios necesita cambios. Confirmado que `update_product`
  ya re-deriva (su doc comment en `src/services/inventory.rs:410-414` anticipa
  esta feature) y que el test `patching_cost_price_recomputes_the_derived_price`
  (`:1796`) es la prueba de ese camino. `reference_cost` existe pero no lo expone
  ninguna ruta.
- Decisiones 5 y 6 registradas: permiso `InventoryWrite` con botón siempre visible,
  y cerrar F1 antes de ramificar F2.
- Hallazgo que cambia expectativas: **editar línea no tiene UI**, así que el aviso
  solo aparece al agregar. Y `e2e/tests/test_purchases.py` no existe.
- Próximo paso: mergear F1, ramificar F2 desde `main`, y fijar T1+.
- **F1 mergeada a `main`** como `40ad8f7` (merge commit, sin pushear). `main`
  quedó con el árbol exactamente igual al que se validó (`git diff main
  feat/product-markup-pricing` vacío).
- **F2 ramificada desde `main`**: `feat/cost-price-freshness`. Baseline:
  `cargo test` 777 passed, `cargo check --all-targets` 0 errores.
- T1–T8 fijadas. Se arranca por la Señal B (el badge): es la más chica, no depende
  de la Señal A, y es la que cubre al operador que confirma sin mirar el aviso.
- **T1 cerrada** → `c123678`. `cargo test` 782 passed (777 + 5), 0 errores, solo
  los dos archivos permitidos. La verificación independiente confirmó las tres
  compuertas, que `reference_cost` se reusa sin reimplementar la regla, que el test
  de preferido-vs-más-barato realmente discrimina, y que todos los tokens de clase
  existen en `static/tailwind.css`.
- **Seguimiento que dejó esa verificación (no bloqueante)**: abrir el drawer corre
  `list_by_product` **dos veces** — una directa en `product_detail_html` para las
  filas, y otra adentro de `reference_cost`. Es una query indexada de más, en un
  camino que no es caliente (abrir un drawer). Arreglarlo bien es extraer un
  `reference_cost_from(costs: &[ProductSupplierCost])` puro en `suppliers.rs` y
  pasarle los costos ya fetcheados. **Toca `suppliers.rs`, así que no se cuela
  acá**: merece su propio review. Queda anotado para decidir si entra en una slice
  propia o se deja como deuda declarada.
- **Nit cosmético a limpiar**: el comentario del template dice `(cost-freshness S1)`
  y el documento numera la slice como T1. Alinearlo cuando se vuelva a tocar ese
  template, no vale un write propio.
- **Señal B cerrada** (T1 + T2): el badge permanente está implementado, testeado a
  nivel de ruta y cubierto desde el navegador. La rama
  `feat/cost-price-freshness` está pusheada a `origin` y trackea la remota; el
  remoto quedó con la protección respetada (no se tocó `main`).
- Sigue la **Señal A** (T5–T6): el botón que aplica el costo y el e2e del flujo.
- **T3 cerrada** → `d55ed00`. 787 passed. Hubo un incidente a reportar: el writer
  **falló sin producir reporte** y dejó tres bindings de test sin usar que subían
  los warnings de 55 a 58. Nada se dio por bueno: se revisó el diff, se sacaron los
  bindings, y la verificación independiente confirmó las dos compuertas, que los
  cinco tests discriminan, que corren por el constructor real, y —lo que más
  importaba— que **`PurchaseLineView` no llega a ninguna respuesta JSON**: la API de
  compras serializa `PurchaseDetail`, así que el campo nuevo no cambia ningún
  payload. AC10 intacto.
- **T4 cerrada** → `1ac6555`. 790 passed, 55 warnings (baseline), e2e `-k picker` 8.
  La verificación confirmó el encuadre del aviso (dentro de `record_money`, en el
  loop, después de la fila, `colspan` 5 contra 5 cabeceras de Draft), la compuerta
  Draft idéntica a la del botón de borrar, y los 23 tokens de clase presentes en
  `static/tailwind.css` (con método conciente del escape: `.py-2\.5` no lo
  encuentra un grep ingenuo). **Se reveló que strict TDD no se aplicó en esta
  slice** — el template cambió antes que los tests — así que no hay evidencia de
  fase roja; se reportó en vez de inventarla.
- Dos hallazgos de la verificación de T4 que quedan como deuda chica: el comentario
  del test positivo sobreafirma el scopeo (se corrigió en T5), y el aviso deja dos
  líneas de borde en una fila marcada (cosmético, se decidió no tocar la fila
  existente).
- **T5 cerrada** → `844248c`. 797 passed, 55 warnings, guard de wiring verde.
  **Corrección de una afirmación mía**: dije que el guard genérico de wiring iba a
  probar la ruta nueva "sola" y es **falso**. El fixture del guard crea una línea a
  costo 7,50 contra un producto de costo 10, así que el aviso nunca se renderiza en
  las páginas vigiladas y el `hx-post` nuevo **no se prueba nunca**. La ruta igual
  queda cubierta por los 7 tests dedicados que le pegan (200/400/403), así que una
  registración faltante se detectaría igual — pero por los tests, no por el guard.
  Decisión: **no** tocar el fixture compartido (cambiarlo arriesga los otros guards)
  y dejar el hueco de cobertura registrado.
- Otras dos deudas chicas registradas por la verificación de T5, ninguna defecto: el
  handler duplica la comparación de estado de `ensure_draft` (que es privado en
  `src/services/purchases.rs`, fuera del alcance de la slice); y la suite read-only de
  AC10 no incluye la ruta nueva, aunque su compuerta sí está cubierta por un test
  dedicado.
