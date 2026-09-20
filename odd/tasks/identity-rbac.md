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
- [x] S3 — Administración de usuarios + cambio de contraseña obligatorio (T13–T15; AC21 de la
      superficie de usuarios queda para S7, ver S3 part 2 en Progreso)
- [x] S4 — Administración de roles y matriz de permisos (T16–T17)
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

### S3 part 1 — el cambio de contraseña obligatorio (T14)
- Branch `feat/identity-password-change` desde `main` limpio (`5067def`). El flag `must_change_password`
  ya no es decorativo:
  - **El confinamiento vive en el middleware** (`guard.rs`), después de la resolución de sesión y antes
    de la lectura de permisos (el confinamiento no es una pregunta de permisos: rige para todo principal).
    Mismas tres formas que el portón, apuntadas a `/password`: página completa `303`, `/api/*` `403` JSON
    con motivo («Se requiere cambiar la contraseña antes de continuar»), `HX-Request` `403` +
    `HX-Redirect: /password`. Sigue alcanzable mientras el flag está puesto: `/password` (GET/POST),
    ambos logouts y la allowlist pública; el origin check y el deny-by-default quedan aguas arriba.
  - **El servicio es la capa que tiene la regla**: `IdentityService::change_password_keep_only_session`
    compone `change_password` (verificar actual, validar la nueva con `MIN_PASSWORD_LEN`, distinta;
    actualizar el hash; limpiar el flag) con la revocación de todas las demás sesiones del usuario:
    `SessionRepository::revoke_all_for_user_except(user_id, keep_token_hash)`, un solo UPDATE con
    `AND token_hash != ? AND revoked_at IS NULL` y el `revoked_at` sellado por el strftime propio de la
    base — sin delete, sin re-insert, sin prune, sin ventana en la que la cookie actuante no nombre
    fila, sin ningún timestamp bindeado por Rust cruzando el borde, y con la fila actuante conservando
    id y expiración (lo que la Fase B va a leer del historial de sesiones). Idempotente como `revoke`:
    la segunda llamada matchea cero filas.
    [Corrección de la primera pasada] La primera implementación expresó "dejar viva esta sesión" con
    revocar-todo → prune → re-insert del mismo digest, porque `session_repo.rs` no estaba en las
    superficies autorizadas del brief. Tenía tres defectos reales en una ruta de seguridad: ventana de
    cero sesiones entre revoke y re-insert, dependencia de la comparación Rust-bindeado vs.
    DB-escrito que produjo el bug F3 (un reloj inyectado desincronizado convertía el cambio de
    contraseña en un cierre de sesión), y una fila de sesión con identidad cambiada sin motivo. Con
    `session_repo.rs` autorizado en la ronda de corrección, el re-seat se eliminó por completo y el
    método del repo quedó como la única expresión del "salvo esta". `revoke_all_for_user` (todo-o-nada)
    queda para la desactivación (S3 part 2) y S4.
  - `GET/POST /password` en `identity_web.rs`: tarjeta en el idioma de login/forbidden (español, notice
    de peligro), campos actual/nueva/confirmación. La confirmación es del formulario; las reglas de
    credencial son del servicio (sus mensajes de validación pasaron a español — hasta ahora dormidos,
    nunca visibles para el operador). Contraseña actual incorrecta: 401 con «La contraseña actual no es
    correcta» y nada escrito (el servicio verifica antes del primer write). La confirmación que no
    coincide, la corta y la igual a la actual: 400 con su motivo y nada cambiado.
  - **Sidebar**: grupo «Account» con la entrada `password` (icono candado heroicons, solo clases
    existentes — no hizo falta recompilar `static/tailwind.css`); la página marca `nav_key = "password"`.
  - **El placeholder de S1b se invirtió, no se borró**: `must_change_password_does_not_confine_the_session_yet`
    → `must_change_password_confines_the_session_to_the_password_change`, mismo fixture
    (`seed_flagged_session`), aserción al revés (303 a `/password` + la página responde 200).
  - Tests nuevos: 15 (5 del confinamiento en `guard.rs`, 2 del servicio sobre el `FakeClock`
    compartido, 6 de la ruta, 2 del método nuevo en `session_repo`). Los de servicio vuelven al
    `FakeClock` una vez eliminado el re-seat: con el método `_except` ningún timestamp bindeado
    compara contra `revoked_at` escritos por la base, así que la determinismia del reloj inyectado
    vale de nuevo en toda la suite.
- Números (ronda de corrección, re-seat reemplazado por `_except`): `cargo test` 481 → **496 passed /
  0 failed** (+15 sobre la base de 481); `cargo check --all-targets` 0 errores, **66 warnings** (68 en
  la base − 4 graduados por S3 part 1 + 2 que vuelven a dormirse con el re-seat eliminado:
  `revoke_all_for_user` del repo y `revoke_all_sessions` del servicio, esperando desactivación y S4);
  grep de allows vacío; `scripts/e2e.sh -k identity` 4 passed.
- Sonda en vivo con el binario real (ronda de corrección): sin `ROYA_ADMIN_PASSWORD`, dos logins del
  admin marcado — actuante `GET /` 303 a `/password`, `GET /password` 200, la otra sesión `GET /` 303 a
  `/password` (viva y confinada), `POST /password` 303 a `/`, actuante `GET /` 200, y la otra sesión
  `GET /` **303 a `/login?next=%2F`** (muerta, sobre HTTP real); con `ROYA_ADMIN_PASSWORD` seteada —
  `GET /` inmediatamente tras login 200 sin confinamiento. Ronda anterior: `GET /api/accounts` marcado
  403 JSON con motivo y `HX-Request` 403 + `HX-Redirect: /password`.

### S3 part 2 — la administración de usuarios (T13 + la parte de usuarios de T15)
- Branch `feat/users-administration` desde `main` (`44325b7`). Consumidora real del kernel de S2:
  - **Service** (`IdentityService`): `create_user` (forma del username y unicidad NOCASE pre-chequeada,
    display name, contraseña inicial con `MIN_PASSWORD_LEN`, el target sale marcado
    `must_change_password` — el administrador eligió la credencial, como en el bootstrap generado),
    `set_user_active` (desactivar revoca todas las sesiones del usuario vía
    `revoke_all_for_user`; idempotente), `admin_reset_password` (marcado sobre el **target**, nunca el
    actor; auto-reset rechazado — ese cambio es `/password`, que verifica la actual; sin revocación:
    las sesiones vivas del target quedan vivas pero confinadas por el portón al próximo request),
    `assign_roles` (`replace_user_roles` con `granted_by` = actor; pre-valida que los roles existan;
    rechaza que el administrador actuante se edite a sí mismo un conjunto sin `identity.roles.manage` —
    la regla de la spec «Role assignment» que ningún trigger cubre), y las lecturas
    `list_users_with_roles` (users.list + list_for_user por usuario) y `role_list`.
  - **Repositorios**: `UserRepository::list` nuevo; `user_repo::set_active` ahora mapea errores (el
    trigger de AC14 llegaba como 500 crudo) y `map_db_err` gana la rama del trigger con su texto
    español («No se puede desactivar: es el último usuario activo que sostiene un rol protegido…»);
    los mensajes UNIQUE y CHECK de users pasaron a español (F4 y la copia operator-facing, ver ronda de
    tests preexistentes abajo). `RoleRepository::delete` nuevo (S4 lo consumirá): mapea el trigger de
    AC13 («No se puede eliminar un rol protegido») y el FK RESTRICT de AC15 («hay usuarios con este rol
    asignado») a Conflict español; el FK en el camino grant/replace pasa a Validation («Uno de los roles
    indicados no existe»).
  - **Pantalla** (`users_web.rs` + `templates/users.html` + `partials/user_list.html`,
    `user_roles_form.html`, `user_password_form.html`), patrón customers exacto: página + fragmento de
    lista + `<dialog>` de creación + segundo `<dialog>` que carga los formularios de roles y reset por
    fragmentos id-finales (`/web/users/roles-form/{id}`, `/web/users/password-form/{id}`), eventos
    `HX-Trigger` (`user-created`/`user-changed`), `data-action` para el notice de éxito, ids en el body
    en los endpoints de colección. En la única asignación de roles las casillas repiten la clave
    `role_ids`, que `Form` (serde_urlencoded) rechaza como campo duplicado: ese handler lee el body crudo
    con un parser propio sin dependencia nueva (los demás formularios siguen con `Form` +
    `#[serde(default)]`). Copia en español como el resto de la familia identity; los `data-action` del
    notice quedan en inglés («Create user saved») porque el sufijo vive en base.html, que esta slice no
    toca.
  - **Gating**: `GET /users`, `GET /web/users` → `Require<IdentityUsersRead>`; las cinco mutaciones →
    `Require<IdentityUsersManage>`. La pantalla lee el `Principal` como `Extension` para `granted_by` y
    para el `can_manage` que esconde los botones. Primera consumición real del extractor y de
    `templates/forbidden.html` por una ruta de producción.
  - **Sidebar**: entrada «Usuarios» (`nav_key = "users"`) en el grupo Account, icono heroicons,
    clases existentes — sin recompilar CSS. Su ocultamiento por permiso (AC21) queda para S7.
- Tests: +16 (5 service, 3 role_repo, 8 route). Los de ruta usan el fixture compartido más un rol
  custom con los dos permisos identity.users otorgado al usuario de prueba, para que el administrador
  de bootstrap siga siendo el único holder protegido y la aritmética de AC14 quede observable.
- AC13/AC14/AC15 por pantalla: el último administrador no se desactiva (409 + mensaje español + nada
  escrito) y un segundo administrador creado por la pantalla desbloquea; el mensaje de rol asignado a
  usuarios existe en `role_repo::delete` (la pantalla es S4). El reset del administrador marca al
  target y no al actor; rechaza el auto-reset y la contraseña corta sin escribir nada.
- **No alcanzado (frontera exacta):** AC21 (ocultar la entrada del sidebar) — deferred a S7 como pide
  el brief; la pantalla de roles y la matriz (S4) — el mensaje de AC15 existe pero no su pantalla; la
  edición del display name (no estaba en los entregables); el e2e de la pantalla en navegador (S8).
- Números: `cargo test` 496 → **512 passed / 0 failed** (+16); `cargo check --all-targets` 0 errores,
  **66 → 60 warnings**; grep de allows vacío; `scripts/e2e.sh -k identity` 4 passed.

### Ronda de corrección S3-ii (verificación adversaria: DO NOT COMMIT — takeover administrativo reproducido)
- El verificador reprodujo con el binario real que un principal con **sólo** `identity.users.manage`
  se autorgaba el rol `admin` (200), creaba cuentas y las volvía administradoras (200), pasaba de 403
  a 200 en lecturas, **reseteaba la contraseña de otro administrador y entraba como él** (200 → login
  303) y le quitaba el rol protegido a otro administrador (200). La regla de self-lockout sólo
  protegía la retención del actor; el caso espejo no existía.
- **Modelo de autorización implementado (decisión de diseño, aplicada tal como se especificó):**
  `identity.users.read` ve la lista; `identity.users.manage` crea usuarios, activa/desactiva, edita
  el display name y **resetea la contraseña de un usuario sin rol protegido**; `identity.roles.manage`
  **cambia el conjunto de roles de cualquiera menos el propio** y resetea la contraseña de quien
  sostiene un rol protegido (tomarse una cuenta que administra la instancia es una decisión sobre la
  administración). **Nadie cambia sus propios roles, con ningún permiso** — una sola regla cierra la
  auto-escalada y el self-lockout. El trigger de último holder protegido queda como backstop de la
  base, no como sustituto. La regla de tier del reset vive en el servicio contra los roles del
  TARGET; el endpoint `/web/users/roles` además se porta a `Require<IdentityRolesManage>`, y el
  servicio repite el chequeo de tier para que no dependa del extractor. La interface esconde el
  botón Roles para la fila propia (`acting_user_id`) y lo separa del tier de usuarios
  (`can_manage_roles`).
- Hallazgos menores cerrados en la misma ronda: `POST /password` comparte el throttle por-username
  del login (misma clave, mismo reloj inyectado; N fallos seguidos rechazan el siguiente intento
  antes de verificar y el éxito limpia el contador); los ids de roles se resuelven en una sola
  statement (`RoleRepository::find_by_ids`, `QueryBuilder` con binds, convención del repo); el body
  sobredimensionado del form de roles responde `413` con la forma JSON de la app en español
  (`AppError::PayloadTooLarge`, límite propio del handler de 64 KiB); `role_repo::map_db_err` mapea
  el trigger de borrado del rol protegido a su conflicto español y la rama FK queda documentada como
  grant-context only (el camino de delete mapea el motivo de los holders); un `user_id` duplicado en
  el form de roles se rechaza (400) en lugar de quedarse con el último valor.
- La spec de S3 escribe el modelo («Admin password reset», «Role assignment» y «Cross-account
  honesty»: qué puede hacerle cada tier a otra cuenta, y que un permiso con consecuencia no escrita
  es uno que un operador no puede otorgar a sabiendas).

### S4 — la administración de roles y la matriz (T16–T17)
- Branch `feat/roles-administration` desde `main` actualizado. Consumidora de la superficie dormida
  de S2/S3: `role_repo::delete` (con sus dos conflictos ya mapeados), `permission_repo::{list,
  set_role_permissions, map_db_err}`, `Role.description` y la estructura `Permission`.
  - **Pantalla** (`roles_web.rs` + `templates/roles.html` + `partials/role_list.html`,
    `partials/role_edit_form.html`), patrón users exacto: página + fragmento de lista + `<dialog>`
    de creación + segundo `<dialog>` que carga por fragmento id-final
    (`/web/roles/edit-form/{id}`) el formulario de detalles Y la matriz. Eventos `HX-Trigger`
    (`role-created`/`role-changed`), `data-action` para el notice («Create role», «Edit role»,
    «Edit permissions»), ids en el body, `#[serde(default)]`. El form de matriz repite la clave
    `permission_ids`, así que ese handler lee el body crudo con su propio límite de 64 KiB y
    responde `413` en español — mismo patrón que el form de roles de la pantalla de usuarios.
  - **Service**: `list_roles_with_holders` (los holders de CUALQUIER estado: el FK RESTRICT no
    distingue activos de inactivos), `create_role` (forma del código `^[a-z][a-z0-9_]*$` 2-64,
    unicidad pre-chequeada, nombre 1-128, descripción opcional ≤ 256), `update_role` (sólo nombre
    y descripción — el código no es campo editable en NINGÚN rol: un nombre máquina no es una
    etiqueta, y así el rechazo de renombrado del rol protegido es inalcanzable por construcción
    desde la interfaz), `delete_role` (rechaza el rol protegido y, si usuarios lo sostienen,
    NOMBRA a los usuarios que bloquean), `role_matrix` (el catálogo completo de 23 con los ids
    sostenidos) y `set_role_matrix` (deduplica y resuelve el conjunto en UNA statement
    — `find_by_ids` nuevo de permission_repo, fijado por un double contando, el mismo patrón de
    `assign_roles` — y rechaza la edición de la matriz del rol protegido).
  - **La regla nueva que cierra el hueco real**: rechazar una edición de matriz que le Quite
    `identity.roles.manage` a un rol que el principal ACTUANTE sostiene. Sin ella, un holder de
    roles.manage se quitaba su propio tier por la matriz y quedaba — junto con todos los que
    comparten el rol — afuera de la administración en el request siguiente: la misma clase de
    lockout que la pantalla de usuarios ya rechaza para los conjuntos de roles. La casilla viene
    pretildada, así que la interfaz nunca ofrece la quitada; el rechazo es real igual.
  - **Gating**: toda la superficie — lecturas incluidas — declara `Require<IdentityRolesManage>`.
    Decisión deliberada escrita en la spec: el catálogo no tiene `identity.roles.read`, y
    inventar un código exige migración; el costo queda dicho: un operador que sólo puede MIRAR
    roles necesita sostener el permiso de administración. Cada mutación repite el chequeo de tier
    en el servicio, como `assign_roles`.
  - **Los dos trigger mappings pendientes de la bitácora, cerrados**: el rechazo de renombrado
    (`protected role code cannot change`) ahora mapea en `role_repo::map_db_err` a su conflicto
    español («No se puede cambiar el código de un rol protegido: su nombre máquina está fijado al
    sembrar.»), y el rechazo de matriz (`protected role permissions cannot be removed`) pasó de su
    texto inglés crudo en `permission_repo::map_db_err` a su conflicto español («No se puede quitar
    permisos a un rol protegido…»). También los CHECK de schema de `roles` (forma del código,
    nombre, descripción) pasaron a español, y el UNIQUE del código es un 409 español de respaldo.
  - **El rol protegido, presentado como bloqueado**: sin botón de baja (la fila ofrece «Ver»), el
    diálogo muestra su matriz sin casillas y sin submit, con la nota de por qué; los handlers
    rechazan igual, con el motivo en español. Su nombre y descripción siguen editables (los
    triggers bloquean el código, no la etiqueta).
  - **Sidebar**: entrada «Roles» (`nav_key = "roles"`) en el grupo Account, icono heroicons
    shield-check, clases existentes — sin recompilar CSS. Su ocultamiento por permiso (AC21) queda
    para S7, como `/users`.
- Tests: +15 (13 de ruta, 2 de servicio). Los de ruta cubren AC17 (tilde de
  `identity.users.read` sobre el rol del propio actor → `GET /users` 403 → 200 → 403 sin restart;
  la quitada legítima mantiene el tier), AC13 (matriz y baja del rol protegido rechazadas con el
  motivo en español y nada escrito; diálogo de sólo lectura; sin Eliminar; sin campo de código),
  AC15 (la baja de un rol sostenido rechazada nombrando a los usuarios), el self-lockout de
  matriz (403, nada escrito, y la misma quitada sobre un rol que el actor NO sostiene sucede), el
  principal sin `roles.manage` rechazado en la página (403 HTML) y en cada mutación (403 JSON con
  el código), las idas y vueltas de creación/edición con el conflicto de código duplicado, las
  formas malformadas, el body sobredimensionado en 413, el `role_id` duplicado rechazado, el
  conjunto presente-vacío como conjunto vacío y el reemplazo del conjunto completo. Los de
  servicio cubren los mismos rechazos en español antes de toda escritura.
- Números: `cargo test` 526 → **541 passed / 0 failed** (+15); `cargo check --all-targets`
  **0 errores, 60 → 56 warnings** (ledger re-medido en tasks.md; los dos dormidos restantes de
  role_repo — `count_active_holders` y `revoke` — no tienen consumidor natural en esta pantalla:
  la baja bloquea por el conjunto TOTAL y el otorgamiento vive en la pantalla de usuarios);
  grep de allows vacío; `scripts/e2e.sh -k identity` 4 passed.
- Sonda en vivo con el binario real (base descartable, `ROYA_ADMIN_PASSWORD` seteada):
  `GET /roles` 200; crear `supervisor` 200 + `HX-Trigger: role-created`; crear usuario y
  asignarle el rol; `GET /users` como ese usuario 403; tilde de `identity.users.read` por la
  matriz 200; el request siguiente del mismo usuario `GET /users` **200** sin restart; baja de
  `vendedor` sostenido → 409 «No se puede eliminar el rol «vendedor»: lo sostienen probesup.
  Primero quitáselo a los usuarios que lo sostienen.»; edición de matriz del rol protegido →
  409 «No se puede editar la matriz de un rol protegido: sostiene la administración de la
  instancia.»; baja del rol protegido → 409; self-lockout de matriz (el holder quitándole
  `identity.roles.manage` a su propio rol) → 403 «No podés quitar «identity.roles.manage» de un
  rol que vos sostenés: te dejaría sin acceso a la administración de roles.»; un principal sin
  `roles.manage`: `GET /roles` 403 HTML («Acción no permitida») y `POST /web/roles/matrix` 403
  JSON con el extractor («Se necesita el permiso «identity.roles.manage»…»). Basura de la sonda
  eliminada.
- **No alcanzado (frontera exacta): nada** — la slice completa entró: pantalla, matriz, reglas,
  gating, sidebar, los dos trigger mappings y los tests. Quedan, por diseño de otras slices:
  el ocultamiento del sidebar (S7), el e2e de navegador de esta pantalla (S8), y el test
  AC11/AC17 de mutación (revertir el chequeo del self-lockout rompe su test — verificado por el
  doble contando y el caso espejo del rol no sostenido).

### S4 — ronda de corrección (re-verificación: COMMIT WITH NOTED RISK, 2026-09-22)
Un hallazgo de documentación (la misma clase que ya mordió dos veces) y tres afirmaciones sin test
que las respaldara. Texto + tests; ningún cambio de comportamiento.

1. **MAJOR (documentación): la spec prometía cerrar la auto-escalada y no lo hace.** La regla de
   no-cambio-de-rol-propio cierra el camino DIRECTO; la matriz reabre el indirecto (un holder de
   `identity.roles.manage` tilda `identity.users.manage` sobre un rol que ocupa, recibe 200, y el
   par queda vivo en el request siguiente — AC17 codifica el auto-otorgamiento como intencional).
   El código está bien y el texto estaba mal: la oración se re-scopó al cambio del PROPIO conjunto
   de roles, y «Cross-account honesty» enuncia la consecuencia en palabras del operador, sin
   suavizar — con el permiso, el holder puede sumar CUALQUIER permiso a un rol que ocupa, con ese
   par puede restablecer la contraseña de un holder protegido, y lo que entrega es la instancia
   entera, no solamente la decisión de quién administra. Releído el resto del bloque: ninguna otra
   afirmación queda contradicha por el camino de la matriz.
2. **MINOR: el rechazo de matriz protegida a nivel de servicio no estaba fijado por test.**
   Desactivar el chequeo `is_system` de `set_role_matrix` deja verde
   `ac13_..._locked_through_the_screen` (el trigger + su mapping lo satisfacen). **El guard se
   queda** — no es un duplicado del backstop: el trigger sólo bloquea la MITAD de quitada del
   reemplazo, mientras el servicio rechaza la clase entera de edición antes de que nada llegue a
   la base, con su propio mensaje (la oración que AC13 fija en pantalla) y su propia precedencia
   (rechaza antes de que la validación de forma conteste por un id inexistente). Test nuevo a
   nivel de servicio contra conjunto vacío, id real e id inexistente; mutación probada (guard
   desactivado → el test nuevo falla, el de pantalla sigue verde).
3. **MINOR: «un holder desactivado es nombrado» estaba afirmado sin test.** `holder_names` no
   filtra `is_active` a propósito; test nuevo en `role_repo` fija el conjunto TOTAL (el holder
   desactivado también se nombra) y la baja que sigue rechazada; mutación probada (agregar el
   filtro `is_active = 1` → el test falla).
4. **NIT: la gate con principal sólo-`users.manage` no tenía test.** Las gates se probaban con un
   principal sin permisos, que no distingue «sin permisos» de «los permisos equivocados». Test
   nuevo de ruta: un principal con `identity.users.read` + `identity.users.manage` pero sin
   `identity.roles.manage` recibe el rechazo en la página (403 HTML nombrando la gate) y en la
   mutación de matriz (403 JSON), llega a `/users` (prueba del fixture) y no escribe nada;
   mutación probada (gate de página cambiada a `IdentityUsersManage` → el test nuevo falla).
- Números: `cargo test` 541 → **544 passed / 0 failed** (+3); `cargo check --all-targets` **0
  errores, 56 warnings** (sin cambio); grep de allows **vacío**.

## Next step
S5: enforcement de finanzas e inventario (T18–T19) — `Require<P>` por acción en las rutas de ambos
departamentos, nav gating y el fragmento 403 para HTMX. La pantalla de roles y su matriz están
entregadas; los dormidos que restan de la lista de S4 (`count_active_holders`, `revoke`,
`Role.{created_at, updated_at}`, `Permission.{action, created_at}`) no tienen consumidor natural en
esta pantalla y quedan anotados en el ledger para el re-measure de S7. La deuda explícita de
navegador sigue: la navegación de `HX-Redirect` sin probarse en navegador (S8).

### S5 — enforcement de finanzas e inventario (T18–T19; ronda de writer, 2026-09-19)
Branch `feat/enforcement-finance-inventory` desde `main` actualizado (checkout ya montado por el
orquestador). Trabajo hecho dentro de las superficies autorizadas (`api.rs`, `web.rs`,
`inventory_api.rs`, `inventory_web.rs`, `test_support.rs`, `authz.rs`, los docs):

- **El helper compartido ahora sostiene TODOS los permisos, pero NO vía el rol protegido.**
  `test_support::seed_session` otorga al usuario de prueba un rol propio (`probe_all`) con los 23
  códigos, por el mismo camino real de grant que usa el bootstrap (`roles.grant`, `granted_by` = el
  propio usuario, idempotente). Desviación del brief registrado con motivo: la letra decía «sostiene
  el rol `admin` protegido», pero sembrar un SEGUNDO holder protegido choca de frente con los
  fixtures de las pantallas de identidad — `identity_web`/`identity_api` bootstrapean al único
  administrador protegido ellos mismos (bootstrap_admin ve 1 holder y NO crea al admin → los logins
  contestan 401: 8 tests en rojo), y los fixtures de `users_web`/`roles_web` documentan a propósito
  que el administrador de bootstrap quede como ÚNICO holder protegido para que la aritmética de AC14
  sea observable. Medición con mutaciones del fixture, ambas variantes:
  - grant del rol protegido `admin` → 28 fallas: identity_api (2), identity_web (6), roles_web (5),
    users_web (8), authz (7→0 tras apuntarlos al seed sin roles).
  - rol propio con 23 códigos (implementado) → 11 fallas: SOLO users_web (6) y roles_web (5), todas
    por la misma premisa de fixture (el usuario compartido deja de estar sin permisos).
  El usuario aprobó la variante implementada (Opción 1) y su argumento: sembrar el rol protegido
  en el usuario de test cambia la semántica que los tests de identidad están verificando; un rol
  propio con los 23 códigos por el mismo grant no la toca y deja al bootstrap como único titular
  protegido.
- **Corrección de fixtures autorizada (wiring ONLY):** `test_pool()` de `users_web.rs` y
  `roles_web.rs` pasó a `seed_session_without_roles` — sin tocar ninguna aserción, status esperado
  ni chequeo de cuerpo, sin renombrar ningún test. Es exactamente la premisa que esas pantallas
  venían usando desde S3/S4; los caminos felices siguen otorgando sus conjuntos por
  `app_with_permissions`.
- **Nuevos helpers** en `test_support`: `seed_session_without_roles` (el fixture sin permisos que el
  núcleo y las pantallas de identidad necesitan), `seed_session_with_permissions(pool, codes)` (un
  principal adicional con un conjunto EXACTO de permisos, usernames/roles únicos por llamada) y
  `cookie_for(token)`. El test de plumbing del helper extiende sus aserciones: el principal compartido
  resuelve 23 códigos por la resolución efectiva real del middleware.
- **`authz.rs`: 3 fixtures del núcleo apuntados a `seed_session_without_roles`** (cambio de call-site,
  no de aserciones): `guarded_app` (los tests de AC10/AC11 ejercitan exactamente QUÉ roles sostiene el
  principal), `a_missing_principal_fails_closed` y el test del repositorio de roles (cuenta cero
  holders de `admin`).
- **Las 46 rutas de ambos departamentos anotadas** (10 finance API + 9 finance web + 17 inventory
  API + 17 inventory web; tabla completa y decisiones en `tasks.md`, sección S5). Sin guard
  router-level en ningún módulo (mezclan capacidades); el drawer de producto usa DOS extractores
  (`inventory.read` + `purchases.costs.read`) porque renderiza datos de dos dueños.
- **16 tests nuevos** de enforcement (AC10 sobre los handlers reales): lecturas permitidas con
  rechazos por código en las tres formas (JSON para `/api/*`, JSON para HTMX vía el notice, HTML de
  página completa), escritura-cero tras rechazo (un movimiento de stock y una transacción), el
  gate de lectura del drawer por `purchases.costs.read`, holder-con-permiso con estado normal, y
  el orden de los portones (anónimo → 401 JSON / 303 a `/login`, nunca el 403 de permisos).
- **Evidencia de mutación** (los tests muerden): quitar `Require<InventoryWrite>` de
  `web_create_product` rompe su test de rechazo HTMX; quitar `Require<FinanceWrite>` de
  `create_transaction` rompe el test de rechazo JSON. Restaurados, ambos vuelven a verde.
- Números finales tras la corrección autorizada: `cargo test` 544 → **560 passed / 0 failed**
  (+16 enforcement tests; los 11 fixtures volvieron a verde con wiring only); `cargo check
  --all-targets` 0 errores, **56 warnings — [corregido en la ronda de verificación: el "58 contra
  58" que se anotó acá era un artefacto del método: contaba las dos líneas de resumen
  (`generated N warnings` por target); el conteo real, líneas de lint menos resúmenes, es 56 en la
  rama y en `main`, y el 56 del brief estaba bien]**; grep de allows vacío;
  `scripts/e2e.sh -k identity` 4 passed; **`-k filters` 7 passed y `-k products` 13 passed /
  1 skipped** (el skip es el probe de screenshots opt-in, no un fallo): el enforcement quedó
  delante de esas pantallas y el harness (login del admin de bootstrap) no necesitó debilitar nada.
- **Sonda en vivo con el binario real** (base descartable, `ROYA_ADMIN_PASSWORD` seteada):
  login del administrador 303; `GET /` 200; `GET /products` 200; `GET /web/accounts` 200;
  `POST /web/accounts` 303 (mutación OK); `POST /api/transactions` 201. Caso negativo end-to-end:
  usuario `probe` creado por pantalla + rol `vendedor` asignado + cambio de contraseña (sale del
  confinamiento) → `GET /products` 200 (sostiene `inventory.read`), `POST /web/accounts` **403 HTML**
  («Acción no permitida» / «finance.methods.manage»), `POST /api/products` **403 JSON**
  («Se necesita el permiso «inventory.write»…»), `POST /web/products` con HTMX 403,
  `PUT /api/accounts/1/payment-methods` 403, `GET /api/accounts` 403; el admin sigue 200.
  Basura de la sonda eliminada (scratch de /tmp).
- Sidebar sin tocar (S7), `e2e/` sin tocar, rutas de ventas/clientes/compras/proveedores sin tocar.

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

### S3-ii — segunda ronda de corrección (re-verificación: COMMIT WITH NOTED RISK, 2026-09-21)
La verificación cerró el takeover sin defecto de comportamiento y dejó cuatro ítems de documentación/UX,
resueltos así (sin commits: el orquestador maneja el git):

1. **La especificación sobreprometía (MINOR, contrato del operador).** «Cross-account honesty» decía que
   `identity.roles.manage` podía «tomar cualquier cuenta reseteando la contraseña — incluso la de un
   holder protegido», pero el endpoint de reset está gated `identity.users.manage`: quien tiene solo
   roles.manage no llega. El texto reescrito enuncia la puerta y la regla de tiers (la regla del tier
   dentro del servicio sigue real): el reset de un holder protegido exige los DOS tiers. Releído el
   resto de las reglas reescritas: ninguna otra oración promete un poder que una gate no entrega.
2. **Descripción persistida contradictoria con su propia gate (hallazgo nuevo, dato sembrado).**
   `identity.users.manage` se describía «Crear usuarios, asignar roles y restablecer contraseñas», pero
   asignar roles exige `identity.roles.manage` — y esa descripción es lo que el operador va a leer en la
   matriz de S4 antes de otorgar. Corrección en el catálogo del código + migración nueva
   `20240101000029_clarify_identity_permission_descriptions.sql` (no se tocó la 27, ya mergeada): la
   descripción de `identity.roles.manage` también se completó (su gate cubre cambiar roles de otras
   cuentas). Auditoría de las otras 21 descripciones: sin contradicciones, se dejaron como estaban.
   El test de drift de AC12 ahora compara descripciones además de códigos: espejo
   `PERMISSION_DESCRIPTIONS` en `authz.rs` + test de mutación de descripción unilateral.
3. **«One statement» sin test que lo respalde.** `find_by_ids` solo tenía test del conjunto devuelto:
   un loop por id con la misma semántica quedaba verde. Test nuevo a nivel de servicio con un double
   contando (mismo patrón que `FailingInsertSessions`): la llamada a `find_by_ids` debe ser exactamente
   UNA y llevar el conjunto deduplicado completo.
4. **Forma vacía bien formada contestaba 400 (NIT).** `role_ids=` (clave presente, valor vacío) era
   rechazado como id inválido; la UI omite la clave cuando no hay tilde. Ahora el valor presente-vacío
   es el conjunto vacío; los valores malformados (`%`, `%zz`, no numéricos) siguen rechazados. El
   backstop real sigue siendo el trigger del último holder.

- Números: `cargo test` 523 → **526 passed / 0 failed** (+3: el test de mutación de descripción, el
  double contando de `find_by_ids`, y el de la forma vacía); `cargo check --all-targets` **0 errores,
  60 warnings** (sin cambio neto: la superficie nueva es test-montada y consume ítems ya contados);
  grep de allows **vacío**; migración 29 aplicada a base fresca (1..29) y a base con 27 ya aplicada
  (1..28 + 29): ambos extremos idénticos (23 filas, descripciones corregidas); drift probado fallando
  con una descripción cambiada de un solo lado (falla nombrando el código y las dos cadenas) y
  restaurado en verde; `scripts/e2e.sh -k identity` 4 passed.

## DÓNDE SE FRENA EL FEATURE (post-S5, 2026-09-19) y cómo se retoma
El orquestador frena el feature después de esta slice. Estado al frenar:

**Entregado (Fase A):** S1a (núcleo del kernel), S1b (portón + login/logout + plumbing de test),
S2 (núcleo RBAC: catálogo, guardas, `Require<P>`, resolución efectiva por middleware, drift test),
S3-i (cambio de contraseña obligatorio), S3-ii (administración de usuarios + ronda de corrección),
S4 (administración de roles + matriz + ronda de corrección), **S5 (enforcement de finanzas e
inventario — esta slice; ocultamiento del nav deferido a S7 por diseño del brief)**.

**Falta (en orden, un PR por slice):**
1. **S6 — enforcement de ventas y clientes** (T20–T21): `Require<P>` por acción; el nav gating
   sigue siendo de S7; tests de AC10/AC21 sobre los handlers reales.
2. **S7 — enforcement de compras, proveedores, identidad y dashboard** (T22–T23): la ÚLTIMA slice
   de enforcement; incluye el ocultamiento del sidebar por permiso (AC21 — exige enchufar el
   `Principal` en cada struct de página, la razón por la que S5/S6 no tocaron el nav) y el
   re-measure del ledger (`cargo check --all-targets` de vuelta a ≤ 56 sin `#[allow]` como
   mecanismo).
3. **S8 — cierre de Fase A** (T24–T25): la slice de navegador para AC22 y las dos deudas de
   navegador arrastradas desde S1b/S3 — la expiración de sesión con `HX-Redirect` en pleno HTMX y
   el formulario HTMX rechazado por permisos, sin probarse todavía en un navegador real — más
   README (sección no-auth, tabla de módulos, migraciones, variables), `env.example`, el promote
   de `openspec/specs/identity/spec.md` y el archivado del change folder.
4. **Fase B — auditoría del actor por departamento** (T26–T31): `created_by`/`updated_by` y su
   visualización, departamento por departamento, cerrando con la sección de auditoría de la spec.

**Pendientes concretos de humano al frenar:**
- La rama remota `feat/roles-administration` sigue en `origin` aunque su PR #48 ya está mergeado
  en `origin/main` (verificado con `git branch -a` el 2026-09-19). Decisión registrada: queda SIN
  borrar; borrarla es decisión del dueño, no tiene nada sin mergear.
- Esta slice NO tiene commit de unidad de trabajo todavía: el writer no commitea; el orquestador
  maneja el git — commitear `feat/enforcement-finance-inventory` como una unidad antes de
  retargetear cualquier PR.
- Al reanudar: `mem_context` + `mem_search` por proyecto/feature, releer
  `odd/tasks/identity-rbac.md` y el change folder; la próxima tarea sin terminar es S6 (T20).

### S5 — ronda de corrección (verificación independiente: COMMIT WITH NOTED RISK, 2026-09-19)
Un MAJOR de cobertura y dos NITs. Texto + tests; ninguna anotación cambió de valor.

1. **MAJOR (cobertura): dos gates sin test que las muerda.** Quitar `Require<InventoryWrite>` de
   `web_edit_product` y `Require<DashboardRead>` del dashboard dejaba la suite ENTERA verde — los
   tests de la primera ronda sólo llegaban a `/web/products` POST, `/web/stock-movements` y
   `/web/product-costs`. Cerrado con tests de rechazo por permiso para cada handler que la primera
   ronda no pinneó, cada uno en la forma que su caller lee. Ninguna anotación necesitó corrección:
   edit/ciclo de vida/baja/categoría son mutaciones de producto (`inventory.write`) y el dashboard
   lee `dashboard.read`. Tests nuevos (6): `the_product_edit_gate_...`, `the_product_activate_gate_...`,
   `the_product_deactivate_gate_...` (además prueba que `is_active` no se voltea),
   `the_product_delete_gate_..._writes_nothing` (forma de página completa + conteo de filas),
   `the_category_gate_...` (conteo de filas) y `the_dashboard_gate_refuses_a_principal_without_it
   _and_opens_with_it` (probe con sólo `inventory.read` → 403 HTML nombrando la gate; probe con
   `dashboard.read` → 200).
   **Tabla de mutaciones de la ronda** (cada gate quitado, su test observado FALLANDO, restaurado;
   el diff final de la ronda es tests-only):
   | # | Anotación quitada | Test que falló | Falla observada |
   | --- | --- | --- | --- |
   | M1 | `web_edit_product`: `Require<InventoryWrite>` | `the_product_edit_gate_...` | 200 con fragmento de lista (el edit corrió) en vez de 403 |
   | M2 | `web_activate_product`: `Require<InventoryWrite>` | `the_product_activate_gate_...` | 200 en vez de 403 |
   | M3 | `web_deactivate_product`: `Require<InventoryWrite>` | `the_product_deactivate_gate_...` | 200 en vez de 403 |
   | M4 | `web_delete_product`: `Require<InventoryWrite>` | `the_product_delete_gate_..._writes_nothing` | **303** (la baja corrió de verdad y redirigió) en vez de 403 |
   | M5 | `web_create_category`: `Require<InventoryWrite>` | `the_category_gate_...` | 200 en vez de 403 |
   | M6 | `web_create_movement`: `Require<InventoryStockWrite>` | `ac10_an_inventory_read_only_principal_is_refused_the_htmx_mutations` (primera ronda) | **404 «product 1 not found»** — el handler corrió en vez del gate |
   | M7 | `dashboard`: `Require<DashboardRead>` | `the_dashboard_gate_refuses_a_principal_without_it_and_opens_with_it` | 200 en vez de 403 |
   Ningún test dejó de morder.
2. **NIT (conteo del ledger): el método de la primera pasada contaba las dos líneas de resumen
   (`generated N warnings` por target), reportando 58.** El conteo real — líneas de lint `warning:`
   menos resúmenes — es **56 en la rama y en `main`**: el brief y la nota de S4 estaban bien todo el
   tiempo. La sección del ledger en `tasks.md` quedó reescrita con el método y el número corregidos,
   y todos los objetivos «≤ 58» pasaron a «≤ 56». Delta de la ronda: 0.
3. **NIT (sobre-gating documentado a medias): un principal con SÓLO `finance.methods.manage` es
   rechazado de `GET /api/payment-methods`** — el catálogo de los medios que administra. Quedó
   escrito en las decisiones de mapeo como consecuencia conocida fail-closed: hoy ninguna matriz
   sembrada separa los códigos, y si una matriz futura los separa, la forma correcta es un OR de
   los dos códigos en esa única lectura, no un tipo nuevo del kernel.

Números de la ronda: `cargo test` 560 → **566 passed / 0 failed** (+6, todos mutation-visibles);
`cargo check --all-targets` 0 errores, **56 warnings** (método corregido: sin líneas de resumen);
grep de allows vacío; `scripts/e2e.sh -k identity` 4 passed, `-k products` 13 passed / 1 skipped
(probe opt-in). `git diff --stat` de la ronda: 2 archivos, +238 (sólo tests).
