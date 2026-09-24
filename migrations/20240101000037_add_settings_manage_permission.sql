-- T8: add the dedicated post-setup business-settings permission and grant it to
-- the protected administrator role. This migration does not recreate or alter
-- any identity guard trigger; the INSERT is additive and the existing
-- protected-role trigger continues to refuse every later permission removal.

INSERT INTO permissions (code, module, action, description, created_by)
SELECT 'settings.manage', 'settings', 'manage', 'Administrar la configuración del negocio',
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE)
WHERE NOT EXISTS (
    SELECT 1 FROM permissions WHERE code = 'settings.manage'
);

INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id
FROM roles r
JOIN permissions p ON p.code = 'settings.manage'
WHERE r.code = 'admin'
  AND NOT EXISTS (
      SELECT 1
      FROM role_permissions rp
      WHERE rp.role_id = r.id AND rp.permission_id = p.id
  );
