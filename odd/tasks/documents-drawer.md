# Documents drawer (`/documents` → detalle con acciones)

## Objective
Que cada fila del índice `/documents` se pueda clickear y abra un **drawer
lateral** (el mismo patrón que clientes, proveedores y productos) con **toda la
información del documento** y con las **acciones que existen de verdad** para
esa familia, cada una con su permiso, su estado y un **aviso previo de impacto**
que liste exactamente qué se va a borrar o qué se va a revertir.

Regla del usuario que gobierna el diseño: *acciones reales sin inventar, con
aviso de impacto, y garantizando que no queden pagos colgados ni documentos
asociados al borrado.*

## Problem
Hoy el índice muestra una fila por documento y un link `Open` a la página que lo
posee. Para saber de qué se trata (líneas, pagos, asignaciones, producto y stock
derivado) hay que salir del índice y navegar a otra pantalla, y para operar
(anular un documento que quedó mal, borrar un borrador que nunca se usó) hay que
recordar en qué pantalla vive cada acción. El índice no ofrece ni el detalle ni
la acción en el lugar donde el operador ya está mirando.

## Why
- El índice es la pantalla donde el operador ve *qué pasó*; el detalle y la
  acción deben estar ahí, no a dos navegaciones de distancia.
- El patrón de drawer ya existe y está testeado en tres pantallas: reusarlo es
  barato y consistente.
- La auditoría de actor (M5 Fase B) ya permite decir quién registró y quién
  editó cada documento: la mitad de "toda la info" ya está resuelta.

## Evidence gathered before designing (decisions follow from it)
- **Un borrador no tiene nada colgado.** `record_payment` exige `Confirmed`
  (`src/services/sales.rs:976-980`), y el movimiento de stock y el asiento de
  caja los crea `confirm`. Un borrador tiene sólo sus líneas.
- **Nada más referencia a una venta o una compra que sus hijos CASCADE**
  (`sale_lines`/`sale_payments` → `sales`; `purchase_lines`/`purchase_payments`
  → `purchases`; `migrations/20240101000010`, `...16`, `...17`, `...32`, `...33`).
  Ninguna tabla referencia `stock_movements`.
- **`cancel` es el inverso diseñado y auditado** de un documento confirmado:
  escribe un movimiento `In`/`Sale-return` por cada línea con stock, un `Expense`
  de reembolso por cada pago, los enlaza por `refund_transaction_id` y guarda
  `cancel_reason` + actor (`src/services/sales.rs:1065-1140`). Para un borrador
  es un cambio de estado sin efectos (`:1056-1063`).
- **Borrar un confirmado en cascada dejaría el ledger y la historia colgando**:
  los `Income`/`Expense` quedarían sin su pago, los movimientos de stock como
  historia de un documento inexistente, y **la fila de auditoría desaparecería**
  (no hay tabla de tombstone ni log de eventos en el esquema).
- Los pagos no son documentos independientes: no tienen ninguna ruta propia
  (`grep` sobre los `router()`), sólo viven dentro de su venta/compra.

## Decisions (user-approved)
1. **Alcance de las acciones**: reales, sin inventar. Nada de cascadas para
   documentos confirmados.
2. **Borrar**:
   - **Borrador de venta/compra** → `DELETE` real y nuevo, guardado por
     `status = 'Draft'` en el propio `WHERE`. Es el único borrado honesto que
     existe: por construcción no tiene pagos, ni movimientos, ni asientos.
   - **Confirmado** → **Anular**, con el endpoint que ya existe y está testeado.
     El aviso de impacto lista lo que va a revertir, no un borrado que no va a
     ocurrir.
   - **Pago, recibo y movimiento de stock** → sin borrado, con el motivo escrito
     en el drawer (el dinero ya está en el ledger; la base rechaza un recibo que
     agrupa pagos; la historia de stock es append-only).
3. **Fila de un pago** → el drawer muestra los datos del pago **y** el resumen de
   su documento padre (número, estado, total, pagado, saldo), con las acciones
   del padre. Requiere la primera lectura por pago de la aplicación.
4. **Editar** → el drawer ofrece el botón que lleva a la página del documento,
   donde ya viven los formularios de edición (cabecera en borrador, líneas) y
   todas las acciones multi-campo. No se duplican formularios: duplicarlos
   garantiza drift contra los endpoints existentes.

## Aviso de impacto (lo que pidió el usuario)
Antes de cada acción destructiva el drawer muestra un bloque calculado en el
servidor, con los mismos datos que la acción va a usar:
- **Borrador** → "se elimina el borrador y sus N líneas (listadas arriba);
  nunca se confirmó: no dejó movimientos de stock, ni pagos, ni asientos de
  caja."
- **Confirmado** → "se van a crear N movimientos de stock `In · Sale-return`
  (producto, cantidad), N reembolsos `Expense` por $X a (cuenta), y el saldo del
  cliente vuelve a $Y." Si la anulación tiene una precondición que falla
  (producto inactivo, saldo que quedaría negativo), el aviso lo dice **antes**
  de que el operador se choque con un 400.

## Scope
- **Lectura**: rutas de detalle por familia (`/web/documents/detail/{kind}/{id}`),
  ensamblado de nombres en la capa de rutas (actores, productos, proveedor,
  cuenta y medio, cliente), y las lecturas nuevas que faltan (un pago por id, un
  movimiento por id) en los repositorios que ya son dueños de esas tablas.
- **Drawer**: shell `#document-drawer` + `#document-drawer-body` en la página,
  seis parciales de detalle, disparador en la fila (el identificador), script de
  apertura/cierre con Escape, y el aviso de impacto.
- **Acciones**: bloque de acciones por familia con permiso y estado; `Anular`/
  `Descartar` sobre los endpoints existentes; **`DELETE /web/sales/{id}` y
  `DELETE /web/purchases/{id}`** nuevos para borradores (con `delete_draft` en
  repo + servicio, guardado por estado en SQL y por `ensure_draft` en el
  servicio).
- **Docs**: cambiar el non-goal del spec del índice ("no write action lives
  here"), documentar el drawer y las dos rutas de borrado en la tabla ruta→permiso
  del spec de identidad, y el bloque del README.
- **Tests**: Rust por familia (drawer, narrowing de permisos, estado de la
  acción, borrado de borrador y sus negativas), guard de wiring, y el caso de
  navegador (abrir el drawer, borrar un borrador, ver el aviso de anulación).

## Non-goals
- **No** hay cascada ni borrado para documentos confirmados: su inverso es
  `cancel` (trazable y auditado).
- **No** se borran movimientos de stock (append-only) ni pagos (el asiento queda
  en el ledger).
- **No** se expone el borrado de recibos: la base lo rechaza mientras agrupe
  pagos.
- **No** se duplican formularios de edición en el drawer.
- **No** hay permiso nuevo, ni migración, ni endpoint REST nuevo (la acción es
  de interfaz).
- **No** se toca el ledger, el stock derivado ni la numeración.

## Acceptance criteria
- AC1 Clickear el identificador de una fila abre el drawer con el detalle de esa
  familia; Escape lo cierra; el drawer vacía su cuerpo al cerrarse.
- AC2 El drawer se abre para las **seis** familias, y cada familia muestra su
  información completa (venta: líneas, pagos, totales, actor; compra: ídem con
  proveedor; pago: sus datos + el resumen del padre; movimiento: producto, tipo,
  motivo, cantidad, stock derivado; recibo: cliente, medio, asignaciones).
- AC3 El drawer sólo se abre si el principal puede leer esa familia: un
  `sales.read`-only recibe el 403 estándar en el detalle de una compra.
- AC4 Cada botón sólo se renderiza con el permiso que su endpoint exige
  (`sales.create`/`purchases.create` para el borrado del borrador,
  `sales.cancel`/`purchases.cancel` para anular/descartar) y con el estado
  correcto (borrador vs confirmado).
- AC5 El botón de borrar borrador existe **sólo** para borradores; el servicio
  responde 400 para cualquier otro estado, y el `WHERE status = 'Draft'` hace
  imposible borrar un confirmado aunque el chequeo del servicio se relaje.
- AC6 El borrado de un borrador elimina el documento y sus líneas (CASCADE) y
  nada más: un test prueba que no queda ninguna fila huérfana y que un
  confirmado no se puede borrar.
- AC7 Antes de borrar o anular, el drawer lista el impacto (líneas a borrar, o
  movimientos/reembolsos/efecto en el saldo a crear), calculado en el servidor.
- AC8 Después de una acción el listado se refresca y el drawer se cierra (o se
  recarga si el documento sigue existiendo).
- AC9 Ninguna familia sin acción muestra un botón muerto: en su lugar dice por
  qué no hay acción.
- AC10 El spec del índice deja de decir que no hay acciones de escritura y
  describe la regla real; la tabla ruta→permiso incluye las dos rutas nuevas.
- AC11 `cargo test` verde, `cargo check --all-targets` sin errores, el guard de
  wiring cubre los `hx-*` nuevos, y el caso de navegador queda escrito (y
  ejecutado si el harness está disponible en la máquina).

## Tasks
- [x] T1 — Lectura: lecturas faltantes + ensamblado del detalle por familia +
      ruta del drawer + narrowing por familia + tests.
- [x] T2 — Drawer: shell, parcial genérico, disparador de fila, scripts, y los
      tests de render por familia.
- [x] T3 — Acciones: bloque por familia con permiso y estado, `Anular`/
      `Descartar` sobre los endpoints existentes, `delete_draft`
      (repo+servicio+rutas web) y el aviso de impacto.
- [x] T4 — Docs: spec del índice (non-goal), tabla ruta→permiso, README.
- [x] T5 — Verificación: mutación de los guardas nuevos + auditoría read-only.

## Commits (rama `feat/documents-drawer`)
- `00b7603` D1 — el drawer (lectura): ruta, ensamblado por familia, parcial
  genérico, disparador de fila, scripts, `find_payment`/`get_movement`,
  `DocumentKind::parse`/`read_code`.
- `cd9e87c` D2 — las acciones: bloque por familia con permiso y estado,
  `delete_draft` (repo+servicio+rutas), `Anular`/`Descartar`, el aviso de
  impacto, el spec del índice y la tabla ruta→permiso.
- `e0d0137` D3 — cobertura de navegador (6 tests) + los conteos declarados.
- `5e41ae7` D4 — los hallazgos de la auditoría: links del drawer gateados por el
  código de su ruta destino, el campo oculto del form de anulación ejercitado,
  el stylesheet regenerado, el `data-action` de cada control, y la regresión
  que mi propia indicación causó en el aviso de error (la atajó un test de
  navegador preexistente).
- `c5afca9` D5 — defecto preexistente que la auditoría encontró: la anulación
  se pre-valida en AGREGADO por cuenta y una anulación a medio aplicar se
  rechaza en vez de duplicarse (invariante 10).

## Verification evidence
- `cargo test` (runner de `openspec/config.yaml`): **741 passed, 0 failed**
  (línea base al empezar: 698). `cargo check --all-targets`: 0 errores, sin
  warnings nuevos.
- `scripts/e2e.sh`: **70 passed, 4 skipped** (línea base 62 passed, 4 skipped).
- **Mutación 5/5**: (M1) quitar `AND status = 'Draft'` del `DELETE` → fallan los
  dos tests de repositorio que borran un confirmado/anulado directo, con la fila
  sobreviviendo; (M2) ignorar el permiso en el bloque de acciones → falla el
  test que exige que un principal sin `sales.create` no vea el botón; (M3)
  quitar el narrowing del detalle → el smoke pasa de 403 a 200; (M4) saltear las
  líneas con stock en el preview → falla el test que exige la línea
  `In · Sale-return` y el aviso de producto inactivo; (M5) borrar el listener
  `sale-changed` de la página → falla el test de navegador (y `cargo test` sigue
  verde, que es exactamente por qué la capa browser existe). Árbol revertido y
  limpio tras cada mutación.
- **Auditoría independiente read-only**: PASS en panel, drawer por las seis
  familias, narrowing, gate de acciones, verdad del preview, seguridad del
  borrado, reacción de la página, frontera AC20 y los conteos declarados. Siete
  hallazgos, todos cerrados: los links sin gatear (D4), el campo oculto sin
  ejercitar (D4), el stylesheet vencido (D4), el binding muerto y la etiqueta
  que prometía un recibo (D4), el copy del borrador, el `maxlength` y el
  `data-action` (D4), y el defecto preexistente de la anulación en agregado
  (D5).
- Evidencia del repositorio sobre un Confirmado (la garantía pedida):
  `delete_draft_called_directly_on_a_confirmed_sale_returns_false_and_the_row_survives`
  invoca el repo **sin** el guard del servicio: devuelve `false`, la fila
  sobrevive y sus líneas también.
- Residual declarado: el borrado de un borrador no deja rastro (no hay
  tombstone y no hay nada que estampar: un borrador nunca tocó stock, dinero ni
  deuda). Es la razón por la que el borrado sólo existe para borradores. Y la
  anulación sigue siendo no atómica entre módulos (invariante 10): el agregado
  ya se valida antes de escribir y una anulación a medio aplicar se rechaza,
  pero una caída entre la validación y la escritura sigue siendo un residual
  detectado, no imposible.

## Next step
- Push y PR por work unit: decisión del usuario (nada pusheado).
