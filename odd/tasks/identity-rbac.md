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
- [ ] S1 — Fundación de autenticación (T1–T8)
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

## Verification evidence
Pendiente por slice; se registra acá con el comando, el resultado y el hash del commit de la
unidad de trabajo.

## Carga de revisión
14 slices encadenadas; Fase A ~2,600 líneas, Fase B ~1,900. Cada slice supera el presupuesto de
400 líneas por sí sola, así que los PRs encadenados son la regla y no una preferencia. Corte
natural: Fase A entrega un producto coherente (identidad + autorización) y Fase B puede esperar.

## Next step
S1 T1–T2: dependencias, `security/password.rs` y las dos primeras migraciones.
