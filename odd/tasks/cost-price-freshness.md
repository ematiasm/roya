# F2 — Frescura de `cost_price` (costo del proveedor preferido + aviso al cargar compra)

Feature hermana de `odd/tasks/product-markup-pricing.md` (F1). F1 deriva el precio
de venta de `cost_price`; esta feature se ocupa de que `cost_price` **no esté
viejo**. Son independientes: F1 funciona igual sin esto, solo que derivando de un
costo que puede ser antiguo.

## Objective
Que `products.cost_price` refleje el costo real del proveedor preferido, con
aprobación humana explícita: al cargar una compra, si el costo de la línea sube
respecto del costo guardado del producto, el operador recibe un aviso con un
botón para actualizarlo.

## Problem
`products.cost_price` **nadie lo mantiene**. Las compras nunca lo escriben: en la
confirmación actualizan el satélite `product_supplier_costs`
(`openspec/specs/purchases/spec.md:42-45`), y el propio código lo declara:

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
práctica queda congelada en el valor del alta. Con F1 encima, eso significa
derivar precios de venta de un costo viejo — el drift es silencioso.

## Why
El costo de compra es un dato que el sistema ya conoce (vive en el satélite y se
actualiza en cada compra confirmada) pero que no llega al producto. El aviso con
botón cierra ese hueco sin que nadie tenga que acordarse de editarlo.

## Decisiones del usuario
- **Solo el botón escribe `cost_price`.** El flujo de compra sigue sin escribir la
  columna **nunca**: lo que escribe es una acción web explícita que dispara el
  humano. Consecuencia: **la invariante AC10 sobrevive intacta** — spec de
  purchases y su test no se tocan. Se evaluó la sincronización automática en
  `record_cost` y se descartó justamente por eso.
- **Cascada**: cuando `cost_price` cambia, si el producto tiene markup cargado el
  precio de venta **se recalcula solo** (decisión tomada en F1). O sea: el humano
  aprueba el costo, el precio sigue de forma determinista.

## Lo que ya existe (no reinventar)
- **La lectura ya está implementada**: `SupplierService::reference_cost`
  (`src/services/suppliers.rs:233-245`) devuelve exactamente "proveedor preferido,
  si no el más barato, si no `None`". Falta solo la escritura y el botón.
- **La detección de suba también existe**: `price_alert()`
  (`src/services/suppliers.rs:248-257`) con `PriceAlert::Raised`, derivado de
  `previous_cost` vs `current_cost`, y es lo que alimenta la alerta que ya se ve
  en los drawers de producto y proveedor (`suppliers.rs:392`, test
  `ac9_price_alert_is_derived_from_previous_vs_current`).

## Interpretación a confirmar
El aviso compara **el costo de la línea que se está cargando contra
`products.cost_price`** — el costo que el producto tiene guardado hoy. Esa es la
comparación que hace falta para mantener la columna fresca y la que da sentido al
botón "actualizar el costo".

La otra lectura posible (que el *proveedor* subió su precio respecto de su propio
histórico) **ya está cubierta** por `price_alert`/`PriceAlert::Raised` y no
necesita nada nuevo.

## Scope
Pendiente de mapeo. Antes de fijar tareas hace falta un scout read-only del módulo
de compras, porque acá sí se toca una superficie que no está mapeada:
`src/services/purchases.rs` (57.6K), `src/repositories/purchase_repo.rs` (57.6K),
`src/routes/purchases_web.rs`, `src/routes/purchases_api.rs`, y el punto donde
nace el costo de la línea (`add_line`, con el fallback
`None => product.cost_price` en `src/services/purchases.rs:320`).

Punto de partida esperado:
- Aviso derivado al cargar/editar la línea de una compra draft (no almacenado:
  mismo precedente que el resto de las alertas del repo).
- Acción web nueva que aplica el costo, pasando por
  `InventoryService::update_product` con un patch de `cost_price`
  (`src/services/inventory.rs:337`) para que la derivación de F1 se dispare sola y
  la fórmula quede en un solo lugar. **No** un UPDATE directo.
- Sin cambios en la spec de purchases.

## Constraints
- `products.cost_price` es `NOT NULL DEFAULT '0'` — "sin costo" se manifiesta como
  `0`, no como NULL. El aviso y el botón tienen que tratar el 0 como "no hay
  costo todavía", no como un costo real.
- Si el producto tiene markup y no tiene costo, F1 rechaza la derivación
  (ver AC de F1). El botón de acá es el camino que saca al producto de ese estado.
- strict_tdd: true (`openspec/config.yaml`); sin CI, las suites corren local.

## Acceptance criteria
- [ ] Al cargar una línea de compra con costo **mayor** que `products.cost_price`
      del producto, aparece el aviso con el costo actual y el nuevo.
- [ ] El aviso **no** aparece si el costo es igual o menor, ni si el producto no
      tiene costo guardado (`0`).
- [ ] El botón actualiza `cost_price` y, si el producto tiene markup, el
      `sale_price` derivado queda recalculado.
- [ ] La compra **nunca** escribe `products.cost_price` por sí sola: el test AC10
      sigue verde sin modificaciones, y la spec de purchases no cambia.
- [ ] `cargo test` verde y `cargo check --all-targets` sin errores.
- [ ] `scripts/e2e.sh -k purchases` verde.

## Applicable checks
- `cargo test`, `cargo check --all-targets`
- `scripts/e2e.sh -k purchases`
- `scripts/build-css.sh` si cambian clases en los templates

## Tasks
- [ ] T0 — Scout read-only del módulo de compras: dónde nace el costo de la línea,
      dónde se renderiza la línea del draft, cómo se refresca el fragmento, y
      dónde vive la acción web. Fijar T1+ con eso.
- [ ] T1+ — (a definir después de T0)

## Progress
- Documento creado con las dos decisiones tomadas (botón único escritor, cascada
  automática del precio). Sin código. El detalle de tareas espera el scout de T0.
