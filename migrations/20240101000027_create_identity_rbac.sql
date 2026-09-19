-- Identity RBAC (M5 identity kernel, slice S2). The permission catalog and the
-- seeded roles. The catalog is data seeded by this migration; the application
-- never creates permission rows at runtime, because a permission only exists
-- if code enforces it (`Require<P>` in security/authz.rs). A drift test
-- (security/authz.rs) asserts the seeded catalog equals the catalog compiled
-- into the code, so a code-level permission cannot exist without its row and a
-- seeded row cannot exist without an enforcer. Every seed insert is guarded
-- with INSERT ... WHERE NOT EXISTS so re-running the statements (or replaying
-- them by hand) cannot duplicate a row. `roles.code` is a machine name
-- (^[a-z][a-z0-9_]*$), `roles.name` is the Spanish label the interface shows.
CREATE TABLE IF NOT EXISTS roles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code TEXT NOT NULL UNIQUE
        CONSTRAINT roles_code_shape CHECK (
            length(code) BETWEEN 2 AND 64
            AND code GLOB '[a-z]*'
            AND code NOT GLOB '*[^a-z0-9_]*'
        ),
    name TEXT NOT NULL
        CONSTRAINT roles_name_shape CHECK (length(name) BETWEEN 1 AND 128),
    description TEXT NULL
        CONSTRAINT roles_description_shape CHECK (description IS NULL OR length(description) <= 256),
    is_system INTEGER NOT NULL DEFAULT 0
        CONSTRAINT roles_is_system_flag CHECK (is_system IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_roles_is_system ON roles(is_system);

CREATE TABLE IF NOT EXISTS permissions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code TEXT NOT NULL UNIQUE,
    module TEXT NOT NULL,
    action TEXT NOT NULL,
    description TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS role_permissions (
    role_id INTEGER NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    permission_id INTEGER NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission_id)
);

CREATE TABLE IF NOT EXISTS user_roles (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role_id INTEGER NOT NULL REFERENCES roles(id) ON DELETE RESTRICT,
    granted_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (user_id, role_id)
);

CREATE INDEX IF NOT EXISTS idx_user_roles_role_id ON user_roles(role_id);

-- ---------------------------------------------------------------------------
-- Seeded permission catalog: the 23 codes, byte-identical to the catalog in
-- security/authz.rs. Each insert is guarded so a re-run cannot duplicate.
-- Descriptions are interface copy (the S4 permission matrix shows them).
-- ---------------------------------------------------------------------------

INSERT INTO permissions (code, module, action, description)
SELECT 'dashboard.read', 'dashboard', 'read', 'Ver el panel principal'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'dashboard.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'finance.read', 'finance', 'read', 'Ver cuentas y movimientos'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'finance.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'finance.write', 'finance', 'write', 'Registrar y editar movimientos'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'finance.write');

INSERT INTO permissions (code, module, action, description)
SELECT 'finance.methods.manage', 'finance', 'methods.manage', 'Administrar cuentas y medios de pago'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'finance.methods.manage');

INSERT INTO permissions (code, module, action, description)
SELECT 'inventory.read', 'inventory', 'read', 'Ver productos y stock'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'inventory.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'inventory.write', 'inventory', 'write', 'Crear y editar productos'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'inventory.write');

INSERT INTO permissions (code, module, action, description)
SELECT 'inventory.stock.write', 'inventory', 'stock.write', 'Ajustar stock'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'inventory.stock.write');

INSERT INTO permissions (code, module, action, description)
SELECT 'sales.read', 'sales', 'read', 'Ver ventas'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'sales.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'sales.create', 'sales', 'create', 'Registrar ventas'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'sales.create');

INSERT INTO permissions (code, module, action, description)
SELECT 'sales.cancel', 'sales', 'cancel', 'Anular ventas'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'sales.cancel');

INSERT INTO permissions (code, module, action, description)
SELECT 'customers.read', 'customers', 'read', 'Ver clientes'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'customers.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'customers.write', 'customers', 'write', 'Crear y editar clientes'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'customers.write');

INSERT INTO permissions (code, module, action, description)
SELECT 'customers.collect', 'customers', 'collect', 'Registrar cobros'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'customers.collect');

INSERT INTO permissions (code, module, action, description)
SELECT 'purchases.read', 'purchases', 'read', 'Ver compras'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'purchases.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'purchases.create', 'purchases', 'create', 'Registrar compras'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'purchases.create');

INSERT INTO permissions (code, module, action, description)
SELECT 'purchases.cancel', 'purchases', 'cancel', 'Anular compras'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'purchases.cancel');

INSERT INTO permissions (code, module, action, description)
SELECT 'purchases.costs.read', 'purchases', 'costs.read', 'Ver costos por proveedor'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'purchases.costs.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'purchases.costs.write', 'purchases', 'costs.write', 'Editar costos por proveedor'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'purchases.costs.write');

INSERT INTO permissions (code, module, action, description)
SELECT 'suppliers.read', 'suppliers', 'read', 'Ver proveedores'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'suppliers.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'suppliers.write', 'suppliers', 'write', 'Crear y editar proveedores'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'suppliers.write');

INSERT INTO permissions (code, module, action, description)
SELECT 'identity.users.read', 'identity', 'users.read', 'Ver usuarios'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'identity.users.read');

INSERT INTO permissions (code, module, action, description)
SELECT 'identity.users.manage', 'identity', 'users.manage', 'Crear usuarios, asignar roles y restablecer contraseñas'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'identity.users.manage');

INSERT INTO permissions (code, module, action, description)
SELECT 'identity.roles.manage', 'identity', 'roles.manage', 'Crear roles y editar la matriz de permisos'
WHERE NOT EXISTS (SELECT 1 FROM permissions WHERE code = 'identity.roles.manage');

-- ---------------------------------------------------------------------------
-- Seeded roles. `admin` is the protected role (is_system = 1) and always holds
-- the whole catalog: the statement below grants every seeded permission to it,
-- so a future catalog row reaches the administrator on the same migration that
-- seeds it. The three ordinary roles are editable and deletable; their
-- matrices are the exact sets the change spec fixes. All guarded.
-- ---------------------------------------------------------------------------

INSERT INTO roles (code, name, description, is_system)
SELECT 'admin', 'Administrador',
       'Acceso total. Rol protegido: no se puede eliminar ni reducir sus permisos.', 1
WHERE NOT EXISTS (SELECT 1 FROM roles WHERE code = 'admin');

INSERT INTO roles (code, name, description, is_system)
SELECT 'vendedor', 'Vendedor', 'Ventas y clientes; consulta de stock.', 0
WHERE NOT EXISTS (SELECT 1 FROM roles WHERE code = 'vendedor');

INSERT INTO roles (code, name, description, is_system)
SELECT 'cajero', 'Cajero', 'Cobros en el mostrador y consulta de cuentas.', 0
WHERE NOT EXISTS (SELECT 1 FROM roles WHERE code = 'cajero');

INSERT INTO roles (code, name, description, is_system)
SELECT 'deposito', 'Depósito', 'Stock y compras; consulta de proveedores.', 0
WHERE NOT EXISTS (SELECT 1 FROM roles WHERE code = 'deposito');

-- The protected role holds the entire catalog, whatever it contains by the
-- time this runs.
INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id
FROM roles r
CROSS JOIN permissions p
WHERE r.code = 'admin'
  AND NOT EXISTS (
      SELECT 1 FROM role_permissions rp
      WHERE rp.role_id = r.id AND rp.permission_id = p.id
  );

-- Ordinary role matrices, one statement per role. The codes pick the catalog
-- rows; the NOT EXISTS guard keeps a re-run from duplicating. (SQLite note:
-- a parenthesised VALUES list cannot carry a column alias like PostgreSQL's
-- `AS wanted(code)`, so the codes are filtered with `IN` instead.)
INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id
FROM roles r
JOIN permissions p ON p.code IN (
    'dashboard.read', 'inventory.read', 'sales.read', 'sales.create',
    'customers.read', 'customers.write', 'customers.collect'
)
WHERE r.code = 'vendedor'
  AND NOT EXISTS (
      SELECT 1 FROM role_permissions rp
      WHERE rp.role_id = r.id AND rp.permission_id = p.id
  );

INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id
FROM roles r
JOIN permissions p ON p.code IN (
    'dashboard.read', 'inventory.read', 'sales.read', 'sales.create',
    'customers.read', 'customers.collect', 'finance.read'
)
WHERE r.code = 'cajero'
  AND NOT EXISTS (
      SELECT 1 FROM role_permissions rp
      WHERE rp.role_id = r.id AND rp.permission_id = p.id
  );

INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id
FROM roles r
JOIN permissions p ON p.code IN (
    'dashboard.read', 'inventory.read', 'inventory.write', 'inventory.stock.write',
    'purchases.read', 'purchases.costs.read', 'suppliers.read'
)
WHERE r.code = 'deposito'
  AND NOT EXISTS (
      SELECT 1 FROM role_permissions rp
      WHERE rp.role_id = r.id AND rp.permission_id = p.id
  );
