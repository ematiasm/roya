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
