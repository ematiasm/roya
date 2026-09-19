// Identity kernel: permissions repository (Slice S2). Reads over the seeded
// catalog and the effective-permission join. The catalog is never written at
// runtime (a permission only exists if code enforces it), so this repository
// is read-only by design; the only write-shaped statement in the module is the
// role-permission matrix maintenance S4's editor needs, which is the one place
// a UI legitimately changes authorization.
use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::Permission;

fn row_to_permission(row: &sqlx::sqlite::SqliteRow) -> Permission {
    Permission {
        id: row.get("id"),
        code: row.get("code"),
        module: row.get("module"),
        action: row.get("action"),
        description: row.get("description"),
        created_at: row.get("created_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("protected role permissions cannot be removed") {
        // The guard trigger (AC13), ledger item closed by S4: the matrix
        // editor is the screen path that reaches this statement, and its
        // refusal is the Spanish conflict the interface explains — never the
        // raw trigger text and never an English message the operator reads
        // in the notice box.
        AppError::Conflict(
            "No se puede quitar permisos a un rol protegido: su matriz está protegida por la base de datos.".into(),
        )
    } else if s.contains("FOREIGN KEY constraint failed") {
        // The matrix editor pre-validates the submitted ids in one statement,
        // so a FK here is the backstop for the check-to-write window.
        AppError::Validation("Uno de los permisos indicados no existe.".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait PermissionRepository: Send + Sync {
    /// The whole catalog, catalog order (module, action) — the order the S4
    /// matrix and the drift test rely on.
    async fn list(&self) -> AppResult<Vec<Permission>>;
    /// The union of one user's roles' permissions (AC11): one query, no cache,
    /// so a matrix edit applies to the next request.
    async fn effective_for_user(&self, user_id: i64) -> AppResult<Vec<String>>;
    /// The permission codes one role holds.
    async fn codes_for_role(&self, role_id: i64) -> AppResult<Vec<String>>;
    /// Replace a role's whole permission set (S4's matrix editor; refused by
    /// the guard trigger for the protected role).
    async fn set_role_permissions(&self, role_id: i64, permission_ids: &[i64])
        -> AppResult<()>;
    /// The catalog rows whose ids exist, resolved in ONE statement — the
    /// matrix form's whole submitted set, the same contract
    /// `RoleRepository::find_by_ids` carries for the assignment form: one
    /// round trip, never one query per id, and the caller diffs the submitted
    /// set against the answer.
    async fn find_by_ids(&self, ids: &[i64]) -> AppResult<Vec<Permission>>;
}

#[derive(Clone)]
pub struct SqlitePermissionRepository {
    pub pool: SqlitePool,
}

impl SqlitePermissionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PermissionRepository for SqlitePermissionRepository {
    async fn list(&self) -> AppResult<Vec<Permission>> {
        let rows = sqlx::query(
            r#"SELECT id, code, module, action, description, created_at
               FROM permissions ORDER BY module, action, id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_permission).collect())
    }

    async fn effective_for_user(&self, user_id: i64) -> AppResult<Vec<String>> {
        let rows = sqlx::query_scalar(
            r#"SELECT DISTINCT p.code
               FROM user_roles ur
               JOIN role_permissions rp ON rp.role_id = ur.role_id
               JOIN permissions p ON p.id = rp.permission_id
               WHERE ur.user_id = ?
               ORDER BY p.code"#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn codes_for_role(&self, role_id: i64) -> AppResult<Vec<String>> {
        let rows = sqlx::query_scalar(
            r#"SELECT p.code
               FROM role_permissions rp
               JOIN permissions p ON p.id = rp.permission_id
               WHERE rp.role_id = ?
               ORDER BY p.code"#,
        )
        .bind(role_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn set_role_permissions(
        &self,
        role_id: i64,
        permission_ids: &[i64],
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        // Per-row deletes so the protected-role guard trigger fires for the
        // exact row it refuses; a bulk DELETE ... WHERE role_id = ? would trip
        // it just the same, but per-row keeps the error attributable.
        let current: Vec<i64> =
            sqlx::query_scalar("SELECT permission_id FROM role_permissions WHERE role_id = ?")
                .bind(role_id)
                .fetch_all(&mut *tx)
                .await?;
        for permission_id in current {
            if !permission_ids.contains(&permission_id) {
                sqlx::query("DELETE FROM role_permissions WHERE role_id = ? AND permission_id = ?")
                    .bind(role_id)
                    .bind(permission_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(map_db_err)?;
            }
        }
        for permission_id in permission_ids {
            sqlx::query(
                r#"INSERT INTO role_permissions (role_id, permission_id)
                   VALUES (?, ?)
                   ON CONFLICT (role_id, permission_id) DO NOTHING"#,
            )
            .bind(role_id)
            .bind(*permission_id)
            .execute(&mut *tx)
            .await
            .map_err(map_db_err)?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn find_by_ids(&self, ids: &[i64]) -> AppResult<Vec<Permission>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // One statement, one bind per id (the QueryBuilder keeps every id
        // bound, never interpolated — the sqlx 0.9 audit rule), mirroring
        // `RoleRepository::find_by_ids`.
        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(
            "SELECT id, code, module, action, description, created_at FROM permissions WHERE id IN (",
        );
        {
            let mut separated = qb.separated(", ");
            for id in ids {
                separated.push_bind(*id);
            }
            separated.push_unseparated(")");
        }
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_permission).collect())
    }
}
