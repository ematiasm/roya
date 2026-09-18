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
- `templates/base.html`: `notice()` acepta un link de acción opcional.
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
  `base.html` en `htmx:afterRequest`, así que viaja por
  `HX-Trigger-After-Settle` (el mismo razonamiento que `product-saved`): el
  after-settle es estrictamente posterior al swap y al afterRequest. El
  `HX-Trigger: product-created` actual no se toca.
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
- [ ] T1 — Los cuatro handlers filter-honest: structs + ramas no-drawer por
      `filter_products`, rename del campo propio del modal, `hx-include` en los
      forms, helper de smoke. Con sus tests Rust.
- [ ] T2 — Aviso create-bajo-filtro: derivación server-side, trigger payload,
      link de acción en `notice()`, listener. Con tests Rust y e2e.
- [ ] T3 — Verificación completa (cargo test, check, e2e) + docs + cierre.

## Progress
- 2026-09-18: documento creado, rama `fix/filter-honest-product-mutations`
  (desde `main` limpio en `6b5f025`). Baseline verificado: `cargo test` 350
  passed, árbol limpio, sincronizado con `origin/main`.

## Verification evidence
- (pendiente)

## Next step
- T1: delegar el writer con las superficies de edición exactas.
