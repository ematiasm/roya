# Documents index (/documents)

## Objective
Un panel nuevo en el sidebar, **Documents**, que reúne en un solo listado los
documentos comerciales del negocio — ventas, compras, movimientos de stock,
pagos de venta, pagos de compra y recibos de cliente — con un buscador de texto
y filtros por tipo de documento, usuario (actor) y rango de fechas.

Es un **índice con acceso al detalle**: lee, busca, filtra y linkea a la página
del documento que ya existe. No duplica acciones ni reglas de negocio.

## Problem
Hoy cada familia de documentos vive en su propia pantalla (`/sales`,
`/purchases`, stock dentro de `/products/{id}`, cobros dentro de
`/customers/{id}`). Responder "¿qué pasó el 12 de marzo?", "¿qué registró Ana
esta semana?" o "¿dónde está el comprobante t-123?" obliga a recorrer cinco
pantallas con cinco filtros distintos. No existe ningún lugar que vea el flujo
de documentos completo, ni que permita buscar por número de comprobante a
través de las familias.

## Why
- Una sola pregunta operativa ("¿qué documentos hay en este rango, de este
  tipo, de este usuario?") debe costar una pantalla, no cinco.
- El proyecto ya tiene los datos: la auditoría de actor (`created_by` en las
  tablas de negocio, M5 Fase B) es lo que hace posible "filtrar por usuario", y
  no existía cuando se construyeron las listas por departamento.
- Decisión explícita del usuario: alcance = índice con acceso al detalle;
  tipos = todos los documentos **menos** los movimientos de caja
  (`transactions`); visibilidad = por tipo, con los permisos actuales; volumen =
  tope de últimas N con aviso.

## Scope
- **Kernel (`src/security/authz.rs`)**: visibilidad *any-of* en `NAV_ENTRIES`
  (hoy solo existe *all-of*) y un extractor `RequireAny` para una ruta que se
  abre con cualquiera de varios permisos. Sus tests de drift e invariante.
- **Lectura**: `DocumentKind`, `DocumentRow`, `DocumentFilter`, `DocumentView`
  (`src/models.rs`); una proyección nueva por familia en su repositorio dueño
  (`sale_repo.rs` ×2, `purchase_repo.rs` ×2, `customer_receipt_repo.rs`,
  `stock_repo.rs`); servicio `src/services/documents.rs` que compone, ordena,
  acota y reporta truncamiento.
- **Web**: `src/routes/documents_web.rs` (`/documents` página,
  `/web/documents` fragmento), entrada de sidebar + fila en `NAV_ENTRIES`,
  `templates/documents.html`, `templates/partials/document_list.html`,
  el rewrite de historial de `templates/base.html`, `guarded_pages`.
- **Wiring**: `AppState` + router, altas en `DEPARTMENT_ROUTE_FILES` /
  `DEPARTMENT_SERVICE_FILES` (AC20), y un helper de resolución de nombre→id de
  usuario en `src/routes/mod.rs` (única capa autorizada a leer `users`).
- **Docs**: README, `openspec/specs/documents/spec.md`, y la fila de
  `/documents` en la tabla ruta→permiso de `openspec/specs/identity/spec.md`.

## Non-goals
- **No** incluye `transactions` (movimientos de caja): decisión del usuario.
- **No** incluye acciones de escritura (confirmar, anular, cobrar): cada fila
  linkea al documento, que ya tiene sus acciones.
- **No** incluye paginación real (no existe en el proyecto): tope + aviso.
- **No** agrega ni modifica permisos, ni migraciones: el catálogo de 23 queda
  intacto y no hace falta tocar el censo ni el drift test.
- **No** lista usuarios en un `<select>`: un principal con `sales.read` y sin
  `identity.users.read` no debe recibir el padrón de usuarios. El filtro es de
  texto y la resolución nombre→id ocurre en la capa de rutas.

## Visibility model (decided)
| Familia (`DocumentKind`) | Tabla | Permiso que la abre |
|---|---|---|
| `Sale` | `sales` | `sales.read` |
| `SalePayment` | `sale_payments` | `sales.read` |
| `Purchase` | `purchases` | `purchases.read` |
| `PurchasePayment` | `purchase_payments` | `purchases.read` |
| `StockMovement` | `stock_movements` | `inventory.read` |
| `Receipt` | `customer_receipts` | `customers.read` |

- La entrada de sidebar **y** la ruta se abren con **cualquiera** de
  `sales.read`, `purchases.read`, `inventory.read`, `customers.read`.
- El contenido se angosta por familia: un principal con solo `sales.read`
  recibe las filas de ventas y de pagos de venta, y **ninguna** otra — ni
  siquiera la opción del `<select>` de tipo.
- El filtro de tipo pedido se intersecta con lo permitido; una intersección
  vacía rinde lista vacía, nunca un 403 sobre toda la página.
- La opción de tipo `payments` agrupa las tres familias de pago, y cada fila se
  etiqueta con su origen exacto ("Pago de venta", "Pago de compra",
  "Recibo de cliente").

## Column semantics (decided)
- **Fecha**: única columna común a las seis tablas (`date`); el rango es
  inclusivo en ambos extremos.
- **Usuario**: `created_by` (quien registró el documento). El texto se
  normaliza con `normalize_search` y matchea `display_name` o `username` por
  subcadena; un nombre sin coincidencias rinde lista vacía.
- **Buscador**: número/referencia del documento y nombre de la contraparte
  (cliente/proveedor/producto), case-insensitive, con `like_needle` (escape de
  `\ % _`). Cada familia define sus columnas; el detalle vive en el spec.
- **Monto**: derivado en Rust, **nunca** `SUM()` sobre columnas TEXT (regla del
  proyecto: los importes viven como TEXT y se suman en Rust). Ventas y compras
  suman sus líneas en una consulta por lote (`IN (...)`, sin N+1); los recibos
  suman sus asignaciones. Stock muestra cantidad, no monto.
- **Orden y tope**: fecha descendente, desempate determinístico por `(kind,
  id)` descendente; tope `DOCUMENTS_PAGE_LIMIT = 200` por página, y la página
  dice explícitamente cuando el tope recortó.
- **Límite de lecturas**: la página hace una cantidad **constante** de lecturas
  SQL por familia (≤ 9 en total) sin importar cuántos documentos matcheen. No se
  reusa `list_details_filtered` para ventas/compras porque expande detalles por
  fila (N+1 medido en `sales.rs::list_details_filtered_reads_only_the_result_set`).

## Tasks
- [x] T1 — Kernel: `RequireAny` + visibilidad *any-of* del nav, con sus tests.
- [x] T2 — Lectura: modelos + proyección por repositorio dueño + servicio
      `documents`, con test de lecturas acotadas.
- [x] T3 — Web: ruta, templates, sidebar, `NAV_ENTRIES`, `base.html`,
      `guarded_pages`, helper de usuario en `routes/mod.rs`, smoke tests.
- [x] T4 — Docs: README + specs de openspec.

## Commits (rama `feat/documents-index`)
- `0c2223a` S1 — kernel any-of (`PermissionSet`, `RequireAny`, `NavVisibility`).
- `dd4a8ce` S2 — modelos + familias ventas/compras en sus repositorios.
- `a8673cc` S3 — familias recibos/stock + `DocumentService`.
- `e45233b` S4 — panel `/documents` + `/web/documents`, sidebar, invariante ac21.
- `795a416` S5 — README + `openspec/specs/documents/spec.md` + tabla ruta→permiso.
- `33d72f9` follow-up de revisión — referencia obsoleta en el spec + guard de
  drift que miraba sólo el primer handler.

## Progress
- 2026-09-21: documento creado; rama `feat/documents-index`. Exploración
  cerrada con mapa de convenciones (listas por departamento, filtros
  compartidos, frontera AC20, drift tests de nav/permisos).
- 2026-09-21: T1–T4 implementadas y commiteadas en la rama
  (`0c2223a` S1 kernel, `dd4a8ce` S2 ventas+compras, `a8673cc` S3 recibos+
  stock+servicio, `e45233b` S4 panel web, docs en el commit de T4).

## Verification evidence
- `cargo test` (runner de `openspec/config.yaml`): 698 passed, 0 failed.
  Línea base al empezar: 652. Nuevos tests: 46 entre kernel, modelos,
  repos, servicio, invariante ac21 y smoke.
- `cargo build`: 49 warnings, todos preexistentes (línea base 49 tras S4;
  S2/S3 habían dejado 2 warnings nuevos que S4 eliminó al borrar los
  accessors inventados y la re-export sin uso). Ninguno nombra símbolos
  nuevos.
- Frontera AC20: `documents_web.rs` en `DEPARTMENT_ROUTE_FILES` y
  `documents.rs` en `DEPARTMENT_SERVICE_FILES`; `FROM users` sólo vive en
  `routes/mod.rs` (`audit_actor_names`, `audit_actor_ids`).
- Lecturas acotadas, medidas con los contadores de los repos: ventas 2,
  compras 2, recibos 2 (1 en el repo de recibos + 1 batcheada en el de
  ventas), stock 1; 0 lecturas con `kinds` vacío. Independiente de cuántos
  documentos matcheen (20 documentos en el test).
- Invariante ac21: por cada uno de los cuatro códigos, un principal que
  tiene SOLO ese código abre `/documents` (200) y renderiza su familia
  (presente/ausente por marcador `data-document-group`); el conjunto vacío
  recibe 403 en `/documents` y en `/web/documents` con `HX-Request`.

## Next step
- Verificación independiente: hecha (ver abajo), sin hallazgos bloqueantes.
- Push y PR: decisión del usuario (nada pusheado, nada mergeado).

## Verification rounds
- **Mutación (doctrina de `verification/spec.md`)**: 5/5 guardas
  load-bearing. M1 `permitted_kinds` ignorando permisos → falla el smoke de
  narrowing y el invariante ac21. M2 gate `Require<DashboardRead>` → el
  invariante falla (403 contra 200 con `sales.read`). M3 `truncated=false`
  → fallan el smoke del cap y el test del servicio. M4 `audit_actor_ids`
  anulado → falla el filtro de usuario. M5 orden invertido → falla el test
  de merge. Árbol revertido y limpio después de cada mutación.
- **Auditor independiente (read-only)**: `cargo test` 698 passed por su
  cuenta; veredicto PASS en panel, gate, narrowing por tier, filtros, cap,
  filas/links (los cuatro href contra la gate de su ruta destino), frontera
  AC20 y partición de grupos. Ningún defecto bloqueante ni should-fix; cinco
  nits, dos de los cuales se corrigieron en `33d72f9`.
- **Riesgo residual declarado**: sólo se verificó por lectura (no en
  navegador) el swap HTMX del filtro, el scroll a `#product-{id}` y la
  restauración de historial; y el 400 de claves duplicadas en el query.
