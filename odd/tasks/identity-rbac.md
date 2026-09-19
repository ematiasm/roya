# M5 — Módulo Identidad y RBAC (usuarios, sesiones, roles, auditoría)

Artefactos OpenSpec de este trabajo: `openspec/changes/2026-09-18-add-identity-module/`
(`proposal.md`, `design.md`, `spec.md` con AC1–AC23, `tasks.md` con las 14 slices).
Este documento es la bitácora ODD en español; los artefactos van en inglés por la invariante 6.

## Objective
Que Roya tenga identidad y autorización: usuarios con credenciales, sesiones revocables,
roles cuya matriz de permisos se edita desde la interfaz, un kernel transversal que niega
por defecto y autoriza por acción, y el actor registrado en cada mutación de cada departamento.

## Problem
Hoy no hay identidad de ningún tipo. `README.md` lo declara como característica ("No auth,
local single-user") y eso era cierto mientras lo usaba una sola persona en una sola máquina.
En cuanto la instancia se ve desde un segundo dispositivo de la red del local, `tower-http`
le contesta a cualquiera: quien abre el navegador puede poner precio, cancelar una venta,
pagar a un proveedor y reescribir el mayor de cuentas. No hay tabla de usuarios, ni credencial,
ni sesión, ni middleware, ni forma de decir "el depósito mueve stock pero no ve costos".

Dos consecuencias salen del mismo hueco. Primero, **la autorización no se puede retrofitear
handler por handler sin un kernel**: un chequeo disperso se olvida justo donde importa, así
que el control tiene que vivir en un solo lugar que falle cerrado para las rutas que nadie
se acordó de anotar. Segundo, **los datos registran que algo pasó pero nunca quién lo hizo**:
`sales`, `purchases`, `stock_movements`, `transactions` y los pagos tienen timestamps y no
tienen actor. "¿Quién descontó esto?" y "¿quién canceló esta venta?" hoy no se pueden contestar,
y son las dos primeras preguntas que hace el dueño cuando los números dejan de cerrar.

## Why
Un sistema de departamentos que comparte un dueño y una base, pero no una identidad, tiene un
agujero en el único lugar donde no puede tenerlo: el borde. Y la trazabilidad es el requisito
que hace útil a la autorización, no un extra: permisos sin actor no explican nada.

## Decisión de producto (tomada por el usuario, 2026-09-18)
De las opciones presentadas, el usuario eligió:

1. **Login con formulario + cookie de sesión** (no HTTP Basic, no header de identidad confiable).
2. **Kernel transversal** (middleware + extractor): los departamentos no conocen usuarios.
3. **Roles y permisos editables desde la UI**: el admin crea roles y tildea permisos.
   El **catálogo** de permisos queda sembrado por migración: un permiso existe solo si el código
   lo enforcea, y un test compara catálogo sembrado contra catálogo compilado para que no deriven.
4. **Registrar el actor en cada mutación, y mostrarlo en las pantallas.**

Desviación registrada respecto de la letra de la opción 3: el catálogo de permisos no se crea
desde la UI (serían casillas que no gobiernan nada). Lo editable desde la UI es el rol y su matriz.

## Scope
- `src/security/` (nuevo): `password.rs`, `session.rs`, `authz.rs`, `guard.rs`
- `src/repositories/`: `user_repo.rs`, `session_repo.rs`, `role_repo.rs`, `permission_repo.rs`
- `src/services/identity.rs` (nuevo) + `src/models.rs` (entidades y DTOs)
- `src/routes/identity_web.rs`, `src/routes/identity_api.rs` (nuevos)
- `src/routes/mod.rs` (AppState + middleware), `src/main.rs` (env + CORS), `src/error.rs`
- `migrations/20240101000025…29` (usuarios, sesiones, RBAC, guardas, auditoría por departamento)
- `templates/login.html`, `templates/users.html`, `templates/roles.html`, `templates/password.html`
  + los partials que correspondan
- Fase B: `created_by`/`updated_by` en las tablas de negocio y su visualización
- Fuera de alcance: 2FA, reset por email/token, OIDC/LDAP, registro self-service, permisos a nivel
  fila/campo, pantalla de sesiones activas, tokens de API para máquinas, auditoría de lecturas,
  rate limit por IP, token CSRF en cada formulario

## Constraints
- Invariante 1: sin SQL cross-module. Identidad toca solo sus tablas; ningún departamento consulta
  las tablas de identidad ni recibe `IdentityService`.
- Invariante 2: la flecha va hacia abajo. Identidad no conoce ventas, stock ni cuentas.
- Invariante 6: artefactos en inglés, UI en español. Invariante 7: sin red en runtime.
- Invariante 9: lo que el código no puede garantizar va a la base (triggers de lockout y de
  revocación permanente, validez de sesión evaluada en SQL).
- Invariante 11: la interacción se prueba en el navegador (expiración de sesión en pleno HTMX,
  403 en un formulario HTMX), no con un test de atributos.

## Authorized scope
Módulo nuevo completo (Fase A: identidad + autorización) y auditoría de actor en las tablas de
negocio (Fase B), en slices encadenadas de ~un PR cada una. Sin cambios de reglas de negocio
existentes: la autorización es una capa, no una reescritura de los servicios.

## Acceptance criteria
Los 23 criterios viven en `spec.md` (AC1–AC23) y son la referencia de verificación de cada slice.

## Applicable checks
- `cargo test` (suite completa) al cierre de cada slice
- `cargo check --all-targets` sin errores nuevos
- `scripts/e2e.sh -k identity` para la slice de navegador (S8)
- Verificación independiente por slice (`gentle-ai-verify`) antes del PR de cada una

## Tasks
- [x] S1a — Núcleo del kernel de identidad (T1–T5): deps, migraciones `users`/`sessions`, modelos, repos,
      `IdentityService`, 43 tests propios. Sin router: aditivo.
- [ ] S1b — El wiring: middleware + login/logout + API de sesiones + `test_support` + plumbing de cookie en
      los ~160 tests HTTP existentes + borrar los 15 `#[allow]` temporales (T6, T6b, T7, T8)
- [ ] S2 — Núcleo RBAC: catálogo, guardas, `Require<P>` (T9–T12)
- [ ] S3 — Administración de usuarios + cambio de contraseña obligatorio (T13–T15)
- [ ] S4 — Administración de roles y matriz de permisos (T16–T17)
- [ ] S5 — Enforcement: finanzas e inventario (T18–T19)
- [ ] S6 — Enforcement: ventas y clientes (T20–T21)
- [ ] S7 — Enforcement: compras, proveedores, identidad y dashboard (T22–T23)
- [ ] S8 — Cierre de Fase A: slice de navegador + README + specs (T24–T25)
- [ ] Fase B — Auditoría del actor por departamento (T26–T31)

## Progress
- 2026-09-18: reconocimiento del repo (sin auth, convención de departamentos, invariantes),
  preguntas de diseño respondidas por el usuario, artefactos OpenSpec escritos, rama
  `feat/identity-rbac` creada desde `main` limpio (`38bf6b5`).
- 2026-09-18: **S1a implementada y verificada** por un writer delegado y un verificador independiente
  (`gentle-ai-verify`). División de S1 en S1a/S1b decidida al escribir el brief: S1a es aditiva (el router
  no se toca) y S1b es el cambio rompiente, así que el presupuesto de revisión no se junta en un solo PR.
  El writer frenó una vez con un bloqueo legítimo —`src/security/` no compila sin `mod security;` en
  `src/main.rs`, que mi brief había excluido— y se resolvió autorizando esa única línea.
- 2026-09-18: **S1b partes 1 y 2**, con dos timeouts de runner en el camino: un writer que falló sin escribir
  nada (la slice combinada no entró en una corrida de contexto) y un fix writer que se colgó 30 minutos
  *después* de terminar sus fixes. La slice se partió en tres por eso, y la recuperación del segundo timeout
  no fue adivinar: el estado verificado de S1b-ii quedaba byte-identical en la copia del verificador en
  `/tmp`, así que el diff contra esa copia dijo exactamente qué había cambiado (todo menos FIX-4). FIX-4, un
  test de 15 líneas, lo escribió el orquestador tras los dos fallos del runner.
- 2026-09-18: **Entrega de la Fase A temprana en PRs encadenados.** Issue **#40** (con
  `status:approved`) y cuatro PRs apilados que se mergean de a uno, retargeteando el siguiente después de
  cada merge: **#41** plan (docs, 657 líneas, dentro de presupuesto) → **#42** kernel S1a (3.661, pide
  `size:exception`) → **#43** sesión de test S1b-i (307, dentro de presupuesto) → **#44** portón S1b-ii/iii
  (2.441, pide `size:exception`). Los hijos van en draft a propósito: su base es su padre, así que un merge
  accidental iría a la rama equivocada.
- El split del plan como PR propio no estaba en el plan original: salió de aplicar la regla de chained PRs
  (400 líneas) y ver que el pase honesto de slicing ya se había hecho — S1 → S1a/S1b, y S1b → i/ii/iii. El
  PR del plan es el paso 0 más barato y deja los PRs de código sin prosa.

## Verification evidence
Pendiente por slice; se registra acá con el comando, el resultado y el hash del commit de la
unidad de trabajo.

### S1a — `cargo test` 402 passed / 0 failed · `cargo check --all-targets` 0 errores
- Baseline medido en un clon limpio de `f560e44`: **359 passed / 0 failed**. El 359 del verificador es el
  número autoritativo; los 351/169/324 de observaciones viejas son históricos.
- Primera pasada del writer: 391 passed. Verificación independiente: **PASS-WITH-FINDINGS**, con dos tests
  que no probaban lo que su nombre afirma (probado con mutaciones) y un bug de encoding.
- Ronda de corrección (10 hallazgos): 402 passed / 0 failed, 11 tests nuevos, 5 mutaciones re-ejecutadas.
- Hallazgos que valen la pena recordar:
  - **F3 (bug real, no del brief):** DB escribe `…T00:49:59.249Z`, Rust bindeaba `… 00:49:59`; `'T'` > `' '`,
    así que `revoked_at <= ?` nunca matcheaba una fila del mismo día y `prune` no borraba sesiones
    revocadas hasta el día siguiente. Ahora hay un encoder único en `src/db.rs` y el AC25 lo sella con un
    test que cruza el borde DB↔Rust.
  - **F1/F2 (cobertura falsa):** `ac5_success_clears_the_counter` pasaba con `clear_attempts` en no-op, y
    borrar `AND u.is_active = 1` de `resolve_valid` dejaba la suite verde. Los dos son ahora
    mutation-validados.
  - **F5/F4/F6:** `Debug` filtraba el PHC; input inválido salía como 500 en vez de `Validation`; el bootstrap
    chocaba (`Conflict`) si `admin` existía inactivo.
  - **Deuda de F5:** la slice agrega 15 `#[allow]` que suprimen 51 warnings de bin (54 en `--all-targets`) del
    código nuevo: el «cero warnings nuevos» del writer era cierto *por supresión*, y los dos primeros
    números que circulamos (48) eran de una lectura parcial. S1b los borra: quedó como criterio de cierre T6b.

### Segunda y tercera ronda (verificación → corrección → verificación)
- La re-verificación de la ronda de corrección dio **COMMIT WITH NOTED RISK**: los 10 hallazgos confirmados
  arreglados, y **un defecto nuevo introducido por el arreglo de F8** — el `retain` del throttle conservaba
  para siempre las entradas parciales (`cooldown_until == None`), así que 1.024 usernames distintos saturaban
  el mapa de forma permanente y a partir de ahí las claves nuevas no se trackeaban (brute force sin cooldown).
  Los dos tests de F8 lo ocultaban porque usaban `max_failures: 1`, una configuración incapaz de producir el
  estado que decían acotar. Lo probó con el default real (`max_failures: 5`).
- Ronda final (presupuesto duro de 15 min, reusando el `target/` tibio — la ronda anterior se colgó 30 min
  pagando un rebuild en frío por mutación): el decay por `last_failure` quedó con horizonte configurable
  (15 min, documentado contra el cooldown de 60 s), y **7 mutaciones independientes** confirman que cada
  arreglo rompe un test al revertirlo. Veredicto: **SAFE TO COMMIT**.
- Números del verificador sobre el tradeoff residual: el plateau sostenido por username es **~295 intentos/h**
  y lo fija el cooldown (que ya reiniciaba el contador en la ronda 1), no el decay; un atacante pausado baja
  a ~16-20/h. Sostener la saturación del mapa exige ~85 hashes argon2/s continuos. No es un agujero nuevo.
- Cautela metodológica aprendida: restaurar un archivo con `cp -a` conserva el mtime viejo y cargo puede
  reusar el artefacto mutado en silencio (al verificador le leyó 406/4 en vez de 410/0). `touch` después de
  restaurar, siempre.

### S1b-i — plumbing de sesión de test (`4ead61e`, `f0e2e5d`)
- 411 passed (+1) con **171 tests HTTP re-autenticados y 0 aserciones tocadas**, probado aritméticamente: el
  diff tiene 7 líneas borradas y todas son reescrituras de `Request::builder()` para encadenar la cookie;
  cero líneas de `assert`/`StatusCode` agregadas o borradas. El helper siembra una sesión real por los
  repositorios de producción y ancla el nombre de la cookie con un `const fn assert` + round-trip por el
  parser real.

### S1b-ii — el portón (`433 → 439`)
- Sonda end-to-end con el binario real: `GET /` anónimo → 303 a `/login?next=/`; `/api/accounts` → 401;
  login correcto → 303 + `Set-Cookie: HttpOnly; SameSite=Lax; Path=/; Max-Age=43200`; `GET /` con cookie →
  200; logout → 303 y la fila queda **revocada** (no solo la cookie borrada); cookie muerta → 303;
  cross-origin → 403.
- **El verificador encontró un open redirect explotable (CWE-601) que ni el writer ni yo vimos:**
  `local_next` rechazaba `\`, `\r` y `\n` pero **no TAB**; `next=/\t/evil.com` pasaba la validación, salía
  crudo en `Location: /\t/evil.com`, y Chromium —que borra TAB/CR/LF *antes* de parsear la URL— lo colapsaba
  a `//evil.com` y navegaba fuera del sitio. El test que decía cubrirlo probaba `http://evil.example.com` y
  nada más. Veredicto de esa ronda: **DO NOT COMMIT**.
- Fix: `!next.chars().any(char::is_control)` (regla general, no enumeración de caracteres), `next` emitido
  percent-encoded como query param, tabla de 12 hostiles unitaria **y** end-to-end por `POST /login`, y el
  oráculo de ruta inexistente restaurado para `POST /logout` (que había quedado sondeándose anónimo: un 303
  de rechazo es indistinguible de un 303 de ruta registrada, así que renombrar la ruta dejaba el guard verde).
- Re-verificación: **SAFE TO COMMIT**, con la mitad de navegador reproducida **en Chromium real contra el
  binario real**: todos los casos hostiles terminan en el origen de la app y el servidor señuelo no recibió
  una sola request. Cinco mutaciones confirman que cada test nuevo falla al revertir su fix.
- Corrección de un número mío: predije «−51 warnings» al borrar los 15 `#[allow]`; el resultado neto fue
  +7 (51 → 58), y los 51 previos eran dead code de finanzas/ventas/inventario ajeno a esta slice. Los +7 son
  dead code de *fixtures de test*, no deuda de S2/S3 — eso vive en el target bin.

## Carga de revisión
14 slices encadenadas más S1a/S1b (15 en total): Fase A ~5,500 líneas, Fase B ~1,900. S1a sola midió ~2,500
líneas (2,313 nuevas + 201 modificadas). Cada slice supera el presupuesto de 400 líneas por sí sola, así
que los PRs encadenados son la regla y no una preferencia. Corte natural: Fase A entrega un producto
coherente (identidad + autorización) y Fase B puede esperar.

## Next step
S1b-iii: la API JSON de sesiones (`POST`/`DELETE /api/sessions` + su entrada en la allowlist), el shell propio
para la página de login (hoy el visitante anónimo ve la navegación y el botón de logout), los seis env vars en
`README.md`/`env.example` —con `ROYA_COOKIE_SECURE` marcado como obligatorio en HTTPS—, cobertura de `Secure`
en el borde real, y el arreglo del harness de e2e (que a esta altura ya no puede autenticarse).

Deuda explícita que no se resuelve en S1b-iii: la navegación de `HX-Redirect` sigue sin probarse en navegador
(un test de header es un test de atributo, no de comportamiento; va en S8).

### S2 — núcleo RBAC (T9–T12)
- Migración `create_identity_rbac`: 23 permisos con inserts guardados, `admin` protegido con el catálogo
  completo, y las matrices de `vendedor`/`cajero`/`deposito` exactas al spec. Corrección clave sobre la
  corrida interrumpida: tres statements usaban `JOIN ( VALUES (...) ) AS wanted(code)` — sintaxis de
  PostgreSQL; SQLite no soporta alias de columna sobre una lista VALUES y además faltaba la relación
  `permissions` en el FROM. Reescritos con `JOIN permissions p ON p.code IN (...)` + `NOT EXISTS`;
  verificados statement por statement contra SQLite real (solo 35–37 fallaban; el resto de ambas
  migraciones, incluidos los 5 triggers, ejecutaban limpio).
- `security/authz.rs`: catálogo compilado (`PERMISSIONS`, 23 códigos + censo const), `Principal`,
  `Require<P>` con las tres formas de rechazo, y el drift test de AC12 con ambas direcciones de mutación
  ejerciendo la misma comparación (borrar la fila es imposible por diseño: la FK en cascada dispara el
  trigger del rol protegido — la migración se niega a des-grantar al admin; la mutación legal es renombrar).
- `role_repo`/`permission_repo` + `IdentityService::effective_permissions` (una query, sin cache) y el
  middleware que arma el `Principal` por request.
- Triggers probados con SQL crudo y el texto real de SQLite: los 5 rechazos de AC13/AC14/AC15
  (`SQLITE_CONSTRAINT_TRIGGER`) + un segundo administrador desbloquea los dos que dependen de la aritmética
  de usuarios (el delete/rename/matriz del rol protegido son absolutos).
- Tests: 470 → 474 passed / 0 failed; `scripts/e2e.sh -k identity` 4 passed.

#### D1 — la resolución de permisos del middleware
Ya estaba cableada (la corrida interrumpida la dejó escrita): `auth_middleware` resuelve la sesión, pide el
conjunto efectivo vía `IdentityService::effective_permissions` (una query, sin cache) e inserta el
`Principal` poblado en las extensiones. Lo que faltaba era la prueba de AC11 del lado matriz: nueva
`ac11_a_role_matrix_edit_applies_to_the_next_request_without_a_restart` — edita la matriz de `vendedor`
por el camino real de S4 (`set_role_permissions`) y exige que el siguiente request (sin restart) pase de
200 a 403 y de vuelta a 200. El union-across-roles y el no-roles ya estaban probados por el middleware
(`ac11_permissions_are_the_union...`, `ac11_a_user_with_no_roles_holds_none`).

#### D2 — el administrador de bootstrap ahora sostiene el rol protegido
- `bootstrap_admin` otorga `roles.code = 'admin'` al administrador que crea, al que recupera (inactivo) y
  al caso de upgrade (admin activo sin rol: solo el grant, **sin tocar la credencial** — un reset ahí
  dejaría afuera a un operador vivo al actualizar). Idempotente por el ON CONFLICT DO NOTHING del grant;
  `granted_by` es el propio admin.
- `count_active_admins` (user_repo) migró del atajo por username al join real por roles, como su propio
  comentario de S1a prometía. El comentario quedó actualizado.
- Tests nuevos en `services/identity.rs`: `ac14_a_fresh_bootstrap_administrator_holds_exactly_the_catalog`
  (23 códigos por el join), `ac14_the_trigger_now_protects_the_real_holder` (desactivar al bootstrap admin
  se rechaza con el texto del trigger; con un segundo admin, la desactivación y el borrado del grant
  suceden), `ac14_an_upgraded_active_administrator_gets_the_role_without_a_credential_reset`, y los dos
  tests de recovery de S1a reescritos como fixtures de base actualizada (admin inactivo sembrado por SQL crudo:
  con los triggers vivos ese estado ya no es alcanzable por escrituras legítimas — lo prueban los tests de
  role_repo). Total: 474 passed / 0 failed.
- Consecuencia de warning: el grant real volvió alcanzable la cadena de `role_repo` en el bin
  (trait, struct, `new`, `find_by_code`, `grant`, `row_to_role`, `map_db_err`, `NewUserRole`): 6 warnings
  menos sin `#[allow]`.

#### D3 — número de warnings anotado
`cargo check --all-targets`: **58 en `main` → 68 tras S2**. Sin ningún `#[allow]`; el grep sigue vacío.
El delta es superficie dormida por diseño, con su slice consumidora anotada (tabla completa en
`openspec/changes/2026-09-18-add-identity-module/tasks.md`, sección «S2 warning ledger»): el extractor y
las formas de rechazo (S5–S7), los métodos S3/S4 de los repos, `change_password`/`MIN_PASSWORD_LEN` (S3)
y los structs `Role`/`Permission`. **Requisito registrado: el conteo vuelve a ≤58 al cierre de S7, sin
atributos `#[allow]` como mecanismo** — cada slice consumidora vuelve alcanzable su superficie y S7
re-mide con `cargo check --all-targets`.

### Ronda de corrección del guard (verificación independiente: DO NOT COMMIT)
- **Bypass 1 (bloqueante):** el flip de `roles.is_system` desarmaba toda la garantía: `UPDATE roles SET
  is_system = 0 WHERE code='admin'` [NO-ERROR] y después `DELETE FROM roles` [NO-ERROR] borraban el rol
  protegido y su matriz. Fix: `trg_roles_protected_is_system_immutable` (`BEFORE UPDATE OF is_system`,
  rechaza el flip en ambas direcciones con su propio texto: «protected status is decided at seed time and
  cannot change»). La bandera queda decidida en el seed: la migración 27 inserta los roles con su valor
  final **antes** de que existan los triggers de la 28 — verificado. Re-ejecutadas las dos statements del
  bypass: la primera ahora aborta y el rol y sus 44 filas de matriz sobreviven.
- **Bypass 2 (bloqueante):** `DELETE FROM users` del último admin activo desencadenaba el cascade hacia
  `user_roles` y el trigger de grants evaluaba a mitad de cascade. Fix: `trg_users_last_protected_holder_no_delete`
  (`BEFORE DELETE ON users`, evaluado antes de cualquier cascade, cuando `user_roles` aún tiene las filas;
  texto: «cannot delete the last active user holding a protected role»). El trigger de `user_roles` queda.
  Probado: el delete del último holder se rechaza, sucede con un segundo holder activo, y un usuario sin rol
  protegido sigue pudiendo borrarse. Matiz del fixture: un admin autograntado está además retenido por el
  RESTRICT de su propio `granted_by` (el delete muere por FK, no por el trigger) — la prueba usa grants
  otorgados por un tercero persistente, como el repro del verificador.
- **MAJOR unificado:** `count_active_admins` (que miraba `r.code='admin'`) fue **eliminado** de
  `user_repo`; el bootstrap y los tests usan `role_repo::count_active_protected_holders` — exactamente lo
  que los triggers protegen (cualquier `is_system = 1`). El comentario S1a que prometía el switch quedó
  reemplazado por la nota de unificación. Test nuevo:
  `ac14_the_protected_holder_predicate_agrees_with_the_triggers_across_two_protected_roles` — con un rol
  protegido `dueno`, el servicio cuenta a su holder, los triggers lo protegen (desactivar y borrar se
  rechazan), el bootstrap siembra nada, y un segundo holder levanta los rechazos.
- **MINOR (REPLACE):** los pools que pueden escribir `roles`/`user_roles` toman sus opciones de una única
  constructora `db::base_connect_options` (foreign keys + `recursive_triggers`); `create_pool` la usa y los
  test pools de `test_support`, `authz`, `guard`, `role_repo`, `identity` y `user_repo` pasaron a ella —
  sin copiar listas de opciones. `permission_repo` no tiene pool de test propio (sin módulo de tests).
  Otros pools del repo que tocan esas tablas: los route-tests de `identity_web.rs` e `identity_api.rs`
  escriben en `user_roles` vía el bootstrap que ejercitan — **fuera de las superficies editables de esta
  ronda, reportado, sin tocar**; `smoke_tests` ya tenía el pragma y solo lee (sesiones). Ningún pool de
  departamento alcanza esas tablas (lo sella el grep de AC20).
  Tests nuevos: `an_insert_or_replace_of_a_protected_role_row_aborts` y
  `an_insert_or_replace_of_a_protected_grant_aborts` en `role_repo` — con el pragma compartido, ambos
  REPLACE abortan y la fila/matriz sobrevive.
- **NIT:** `user_repo::map_db_err` mapea el FK crudo («this user granted a role: the grant records who
  granted it, and the deletion is held», 409). Test: `a_grantor_user_delete_maps_the_foreign_key_refusal`.
- Regresión duradera en Rust para los dos bloqueantes:
  `ac13_the_protected_status_flag_cannot_be_flipped` (flip en ambas direcciones rechazado, rol y matriz
  intactos) y `ac14_the_last_active_protected_holder_cannot_be_deleted_by_user_row` en `role_repo`.
- Números: `cargo test` 474 → **480 passed / 0 failed** (+6); `cargo check --all-targets` **68 warnings**
  (sin cambio neto: el conteo no se movió, la tabla dormida se actualizó en ambos documentos); grep
  vacío; `scripts/e2e.sh -k identity` 4 passed.

### S2 — las dos rondas de verificación independiente
- **Ronda 1: DO NOT COMMIT.** Dos maneras de romper la garantía "el administrador siempre existe", y las
  481 pruebas no las veían:
  1. **La protección dependía de una columna escribible.** Todos los triggers miraban `OLD.is_system = 1` y
     nada rechazaba `UPDATE roles SET is_system = 0`: con dos sentencias comunes desaparecían el rol
     protegido y su matriz de 23 permisos, mientras `count_active_admins` (que miraba `code='admin'`)
     seguía reportando "hay 1 admin". Guarda de base y chequeo de servicio divergían justo bajo el bypass.
  2. **El borrado del usuario se llevaba la última asignación.** `DELETE FROM users` sobre el último
     administrador activo: la cláusula `EXISTS(... u.is_active = 1)` es falsa *durante la cascada*, así que
     el trigger no disparaba. Los admins auto-otorgados se salvaban solo por el FK `granted_by RESTRICT`.
  Más un MAJOR (dos predicados para el mismo concepto: `code='admin'` en el servicio, `is_system=1` en los
  triggers) y dos menores (el agujero de `INSERT OR REPLACE` cerrado solo por un pragma por conexión; el
  texto crudo de FK al borrar un otorgante).
- **Corrección:** trigger `is_system` inmutable en ambas direcciones (el flag se decide al sembrar),
  `BEFORE DELETE ON users` evaluado antes de la cascada, `count_active_admins` **borrado** en favor del
  único `count_active_protected_holders` que ya existía, constructor único `db::base_connect_options`, y el
  FK mapeado a un 409 con motivo.
- **Ronda 2: COMMIT WITH NOTED RISK.** Los dos blockers muertos y probados con la tabla de bypass re-corrida
  (estado intacto tras cada intento, incluidos `INSERT OR REPLACE` sobre el rol y sobre la asignación).
  Y por fin la **tabla de mutaciones del enforcement**: las cuatro mutaciones (permiso que permite siempre,
  principal ausente que no falla cerrado, forma del 403 cruzada, unión reducida a un solo rol) rompen tests
  — o sea que los tests de `Require<P>` no son decoración. El verificador no alcanzó a ejecutar el mapeo del
  FK; lo corrí yo (`a_grantor_user_delete_maps_the_foreign_key_refusal`, 1 passed).
- **NIT cerrado por el orquestador:** el verificador probó que un pool construido a mano sin el constructor
  compartido reabre el borrado del último admin (`INSERT OR REPLACE INTO users` con el pragma apagado) — hoy
  inalcanzable, pero es la clase de agujero que vuelve. Se agregó un guard por grep en el módulo de tests de
  `authz.rs` (`every_identity_pool_comes_from_the_shared_connect_options`), en la misma familia que el guard
  de AC20, **validado por mutación**: con el patrón inyectado en `guard.rs` falla nombrando el archivo, y
  restaurado pasa. Es un test agregado después de la ronda de verificación, y queda dicho acá.
- **Números finales de S2:** `cargo test` 474 → **481 passed / 0 failed**; `cargo check --all-targets` 0
  errores, **68 warnings** con el ledger ítem por ítem y la exigencia de volver a ≤58 al cerrar S7 sin
  `#[allow]`; grep de allows **vacío**; `scripts/e2e.sh -k identity` 4 passed.
- **Decisión registrada de paso:** el diseño ahora tiene una fila que compara sesión opaca contra JWT, con
  el motivo (revocación real, cambios de permiso que aplican en el request siguiente, ninguna clave de firma
  que rotar) y la condición para reconsiderar (multi-instancia sin estado compartido, o un cliente externo
  que deba verificar sin tocar la base).
