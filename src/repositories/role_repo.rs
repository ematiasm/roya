// Identity kernel: roles repository (Slice S2; the roles screen writes added
// by S4). One row per role; `is_system` marks the protected role whose
// deletion, rename and permission removals the schema triggers refuse.
// Deletion is only ever attempted through the ordinary `DELETE`: a role still
// assigned to any user is held by the `user_roles` RESTRICT foreign key
// (AC15), and a protected role is held by the guard trigger (AC13) — the
// repository maps both refusals to `Conflict` so the interface can explain
// which users or which rule block the action. S4 closes the two trigger
// mappings this file still owed: the protected-code rename (`protected role
// code cannot change`) and the schema CHECKs the create/edit forms can hit,
// so no statement reachable from the roles screen leaks a raw trigger string
// or an English 500.
use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{NewRole, NewUserRole, Role, RoleGrant};

fn row_to_role(row: &sqlx::sqlite::SqliteRow) -> Role {
    let system: i64 = row.get("is_system");
    Role {
        id: row.get("id"),
        code: row.get("code"),
        name: row.get("name"),
        description: row.get("description"),
        is_system: system == 1,
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        // The S4 create/edit forms pre-check uniqueness, so this branch is
        // the backstop for the window between the check and the write.
        AppError::Conflict("Ya existe un rol con ese código.".into())
    } else if s.contains("protected role code cannot change") {
        // The guard trigger (AC13, ledger closed by S4): the protected role's
        // machine name is decided at seed time. No statement reachable from
        // the roles screen changes a `code` (the edit form edits name and
        // description only), so this mapping keeps any future path — or a
        // script — from leaking the trigger text as a 500.
        AppError::Conflict(
            "No se puede cambiar el código de un rol protegido: su nombre máquina está fijado al sembrar.".into(),
        )
    } else if s.contains("cannot remove the last grant of a protected role to an active user") {
        // The guard trigger (AC14): removing this grant would leave the shop
        // without an active administrator. The interface explains the rule
        // and the way out, never the trigger string.
        AppError::Conflict(
            "No se puede quitar el rol: es la última asignación activa de un rol protegido. Primero asignáselo a otro usuario.".into(),
        )
    } else if s.contains("protected role cannot be deleted") {
        // The guard trigger (AC13), mapped HERE and not only in `delete`:
        // every statement that deletes a role row can hit it — the S4
        // screen's delete route, a script, the delete phase of an INSERT OR
        // REPLACE — and none of them may answer a raw 500 that leaks the
        // trigger text. `delete` maps the same refusal first; this branch is
        // the backstop for every other path.
        AppError::Conflict("No se puede eliminar un rol protegido.".into())
    } else if s.contains("FOREIGN KEY constraint failed") {
        // GRANT-CONTEXT ONLY. The generic SQLite message says nothing about
        // which reference failed, and this repository has two very different
        // refusals behind it: on the grant/replace statements a submitted id
        // names a row that does not exist (the assignment form pre-validates,
        // so this is the honest reason there), while on a role DELETE it is
        // the users still holding the role. `delete` maps the holders'
        // refusal itself and never falls through to this branch; any other
        // DELETE-shaped path must do the same instead of dressing the
        // holders' refusal as a missing id.
        AppError::Validation("Uno de los roles indicados no existe.".into())
    } else if s.contains("CHECK constraint failed") {
        // The create/edit forms validate the same rules in the service (the
        // operator reads them in Spanish before the write); these are the
        // mapped backstops for the statements that reach the schema first.
        if s.contains("roles_code_shape") {
            AppError::Validation(
                "El código del rol debe tener entre 2 y 64 caracteres: sólo letras minúsculas, números y guión bajo, empezando con una letra."
                    .into(),
            )
        } else if s.contains("roles_name_shape") {
            AppError::Validation("El nombre del rol debe tener entre 1 y 128 caracteres.".into())
        } else if s.contains("roles_description_shape") {
            AppError::Validation(
                "La descripción del rol no puede superar los 256 caracteres.".into(),
            )
        } else {
            AppError::Validation("Los campos del rol no cumplen las reglas del esquema.".into())
        }
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait RoleRepository: Send + Sync {
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Role>>;
    async fn find_by_code(&self, code: &str) -> AppResult<Option<Role>>;
    /// Every role, creation order (the S4 list view's order).
    async fn list(&self) -> AppResult<Vec<Role>>;
    /// Roles of one user, by the user_roles join.
    async fn list_for_user(&self, user_id: i64) -> AppResult<Vec<Role>>;
    /// The roles whose ids exist, resolved in ONE statement. The assignment
    /// flow validates a whole submitted set at once (a parser that answered
    /// one query per id let a large form burn one round trip per id); the
    /// caller diffs the submitted set against the answer.
    async fn find_by_ids(&self, ids: &[i64]) -> AppResult<Vec<Role>>;
    /// Count active holders of one role (AC15: the interface names the users
    /// that block a deletion).
    async fn count_active_holders(&self, role_id: i64) -> AppResult<i64>;
    /// Active holders of ANY protected role — the quantity the guard triggers
    /// protect: while it stays above zero, an administrator exists.
    async fn count_active_protected_holders(&self) -> AppResult<i64>;
    /// Record a grant (`granted_by`/`granted_at` are part of the row).
    async fn grant(&self, input: &NewUserRole) -> AppResult<()>;
    /// Remove one grant. The guard trigger refuses the last protected-role
    /// grant of an active user; this repository surfaces that refusal as the
    /// conflict the interface explains.
    async fn revoke(&self, user_id: i64, role_id: i64) -> AppResult<()>;
    /// Delete a role row (the S4 roles screen's delete action). Two things
    /// refuse it and both live in the schema: the guard trigger for a
    /// protected role (AC13) and the `user_roles` RESTRICT foreign key when
    /// any user holds it (AC15). This method is where both refusals are
    /// mapped to the Spanish message the interface explains.
    async fn delete(&self, id: i64) -> AppResult<()>;
    /// Replace a user's whole role set in one transaction (S3's assignment
    /// form posts the complete new set). Returns the roles the user ends
    /// with; refuses (via the trigger) when the change would strip the last
    /// active protected-role holder.
    async fn replace_user_roles(&self, user_id: i64, role_ids: &[i64], granted_by: i64)
        -> AppResult<Vec<Role>>;
    /// Create a role row (the S4 create form). `created_by` is the acting
    /// principal — the database refuses a row without one (NOT NULL) — so a
    /// role created through the screen always names its author. The schema
    /// CHECK is the backstop behind the service's shape validation; the
    /// UNIQUE index backs the pre-checked uniqueness.
    async fn create(&self, input: &NewRole, created_by: i64) -> AppResult<Role>;
    /// Edit a role's name and description (the S4 edit form). The code is
    /// not part of this statement: renames are refused by the guard trigger
    /// for a protected role and are not offered by the interface for any
    /// role (a machine name is not a relabel). Touches `updated_at` and
    /// stamps `updated_by` with the editing actor (slice S13).
    async fn update_details(&self, id: i64, name: &str, description: Option<&str>,
        updated_by: i64)
        -> AppResult<()>;
    /// Usernames of EVERY user holding the role (AC15): the names a blocked
    /// deletion reports. `user_roles.role_id` is ON DELETE RESTRICT for
    /// holders of any state, active or not, so the refusal names all of them.
    async fn holder_names(&self, role_id: i64) -> AppResult<Vec<String>>;
    /// The grant trail of one user (slice S13): every role they hold together
    /// with `granted_by`/`granted_at` — the columns the RBAC slice has
    /// recorded since S2 and the interface never showed. Role order matches
    /// `list_for_user` (creation order).
    async fn list_grants_for_user(&self, user_id: i64) -> AppResult<Vec<RoleGrant>>;
}

#[derive(Clone)]
pub struct SqliteRoleRepository {
    pub pool: SqlitePool,
}

impl SqliteRoleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RoleRepository for SqliteRoleRepository {
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Role>> {
        let row = sqlx::query(
            r#"SELECT id, code, name, description, is_system, created_by, updated_by, created_at, updated_at
               FROM roles WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| row_to_role(&r)))
    }

    async fn find_by_code(&self, code: &str) -> AppResult<Option<Role>> {
        let row = sqlx::query(
            r#"SELECT id, code, name, description, is_system, created_by, updated_by, created_at, updated_at
               FROM roles WHERE code = ?"#,
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| row_to_role(&r)))
    }

    async fn list(&self) -> AppResult<Vec<Role>> {
        let rows = sqlx::query(
            r#"SELECT id, code, name, description, is_system, created_by, updated_by, created_at, updated_at
               FROM roles ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_role).collect())
    }

    async fn list_for_user(&self, user_id: i64) -> AppResult<Vec<Role>> {
        let rows = sqlx::query(
            r#"SELECT r.id, r.code, r.name, r.description, r.is_system,
                      r.created_by, r.updated_by, r.created_at, r.updated_at
               FROM roles r
               JOIN user_roles ur ON ur.role_id = r.id
               WHERE ur.user_id = ?
               ORDER BY r.id"#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_role).collect())
    }

    async fn find_by_ids(&self, ids: &[i64]) -> AppResult<Vec<Role>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // One statement, one bind per id (the QueryBuilder keeps every id
        // bound, never interpolated — the sqlx 0.9 audit rule); SQLite's
        // variable cap (32 766 on the bundled build) sits far above any real
        // submission.
        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(
            "SELECT id, code, name, description, is_system, created_by, updated_by, created_at, updated_at FROM roles WHERE id IN (",
        );
        {
            let mut separated = qb.separated(", ");
            for id in ids {
                separated.push_bind(*id);
            }
            separated.push_unseparated(")");
        }
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_role).collect())
    }

    async fn count_active_holders(&self, role_id: i64) -> AppResult<i64> {
        let row: (i64,) = sqlx::query_as(
            r#"SELECT COUNT(*) FROM user_roles ur
               JOIN users u ON u.id = ur.user_id
               WHERE ur.role_id = ? AND u.is_active = 1"#,
        )
        .bind(role_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn count_active_protected_holders(&self) -> AppResult<i64> {
        let row: (i64,) = sqlx::query_as(
            r#"SELECT COUNT(DISTINCT ur.user_id) FROM user_roles ur
               JOIN users u ON u.id = ur.user_id
               JOIN roles r ON r.id = ur.role_id
               WHERE r.is_system = 1 AND u.is_active = 1"#,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn grant(&self, input: &NewUserRole) -> AppResult<()> {
        sqlx::query(
            r#"INSERT INTO user_roles (user_id, role_id, granted_by)
               VALUES (?, ?, ?)
               ON CONFLICT (user_id, role_id) DO NOTHING"#,
        )
        .bind(input.user_id)
        .bind(input.role_id)
        .bind(input.granted_by)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(())
    }

    async fn revoke(&self, user_id: i64, role_id: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM user_roles WHERE user_id = ? AND role_id = ?")
            .bind(user_id)
            .bind(role_id)
            .execute(&self.pool)
            .await
            .map_err(map_db_err)?;
        Ok(())
    }

    async fn delete(&self, id: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM roles WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                let s = e.to_string();
                if s.contains("protected role cannot be deleted") {
                    // AC13: the guard trigger; absolute, a second
                    // administrator changes nothing.
                    AppError::Conflict("No se puede eliminar un rol protegido.".into())
                } else if s.contains("FOREIGN KEY constraint failed") {
                    // AC15: user_roles.role_id is ON DELETE RESTRICT — the
                    // users holding the role block the deletion (the S4
                    // screen names them; here the reason is named).
                    AppError::Conflict(
                        "No se puede eliminar el rol: hay usuarios con este rol asignado. Primero quitáselo a los usuarios que lo sostienen.".into(),
                    )
                } else {
                    map_db_err(e)
                }
            })?;
        Ok(())
    }

    async fn replace_user_roles(
        &self,
        user_id: i64,
        role_ids: &[i64],
        granted_by: i64,
    ) -> AppResult<Vec<Role>> {
        let mut tx = self.pool.begin().await?;
        // Removals first, one by one: the guard trigger is per-row and must
        // have its say before anything is re-granted.
        let current: Vec<i64> =
            sqlx::query_scalar("SELECT role_id FROM user_roles WHERE user_id = ?")
                .bind(user_id)
                .fetch_all(&mut *tx)
                .await?;
        for role_id in current {
            if !role_ids.contains(&role_id) {
                sqlx::query("DELETE FROM user_roles WHERE user_id = ? AND role_id = ?")
                    .bind(user_id)
                    .bind(role_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(map_db_err)?;
            }
        }
        for role_id in role_ids {
            sqlx::query(
                r#"INSERT INTO user_roles (user_id, role_id, granted_by)
                   VALUES (?, ?, ?)
                   ON CONFLICT (user_id, role_id) DO NOTHING"#,
            )
            .bind(user_id)
            .bind(*role_id)
            .bind(granted_by)
            .execute(&mut *tx)
            .await
            .map_err(map_db_err)?;
        }
        let rows = sqlx::query(
            r#"SELECT r.id, r.code, r.name, r.description, r.is_system,
                      r.created_by, r.updated_by, r.created_at, r.updated_at
               FROM roles r
               JOIN user_roles ur ON ur.role_id = r.id
               WHERE ur.user_id = ?
               ORDER BY r.id"#,
        )
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.iter().map(row_to_role).collect())
    }

    async fn create(&self, input: &NewRole, created_by: i64) -> AppResult<Role> {
        let row = sqlx::query(
            r#"INSERT INTO roles (code, name, description, created_by)
               VALUES (?, ?, ?, ?)
               RETURNING id, code, name, description, is_system, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(&input.code)
        .bind(&input.name)
        .bind(&input.description)
        .bind(created_by)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_role(&row))
    }

    async fn update_details(&self, id: i64, name: &str, description: Option<&str>,
        updated_by: i64)
        -> AppResult<()> {
        sqlx::query(
            r#"UPDATE roles
               SET name = ?, description = ?, updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?"#,
        )
        .bind(name)
        .bind(description)
        .bind(updated_by)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(())
    }

    async fn holder_names(&self, role_id: i64) -> AppResult<Vec<String>> {
        let rows: Vec<String> = sqlx::query_scalar(
            r#"SELECT u.username FROM user_roles ur
               JOIN users u ON u.id = ur.user_id
               WHERE ur.role_id = ?
               ORDER BY u.username"#,
        )
        .bind(role_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_grants_for_user(&self, user_id: i64) -> AppResult<Vec<RoleGrant>> {
        let rows = sqlx::query(
            r#"SELECT r.id, r.code, r.name, r.description, r.is_system,
                      r.created_by, r.updated_by, r.created_at, r.updated_at,
                      ur.granted_by, ur.granted_at
               FROM user_roles ur
               JOIN roles r ON r.id = ur.role_id
               WHERE ur.user_id = ?
               ORDER BY r.id"#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| RoleGrant {
                role: row_to_role(row),
                granted_by: row.get("granted_by"),
                granted_at: row.get("granted_at"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NewUser;
    use crate::repositories::user_repo::{SqliteUserRepository, UserRepository};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;

    async fn pool() -> SqlitePool {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// Create active users through the real user repository and grant each
    /// the protected `admin` role through the real grant path.
    async fn seed_admin_holders(pool: &SqlitePool, names: &[&str]) {
        let users = SqliteUserRepository::new(pool.clone());
        let roles = SqliteRoleRepository::new(pool.clone());
        let admin = roles.find_by_code("admin").await.unwrap().unwrap();
        for name in names {
            let user = users
                .create(
                    &NewUser {
                        username: name.to_string(),
                        display_name: name.to_string(),
                        password_hash: "placeholder-not-a-real-argon2-hash".to_string(),
                        must_change_password: false,
                    },
                    None,
                )
                .await
                .unwrap();
            roles
                .grant(&NewUserRole {
                    user_id: user.id,
                    role_id: admin.id,
                    granted_by: user.id,
                })
                .await
                .unwrap();
        }
    }

    /// The message SQLite put on the refused statement: the raw trigger
    /// refusal, mapped by nothing, so the proof cannot drift from the schema.
    fn refusal_message(err: sqlx::Error) -> String {
        match err {
            sqlx::Error::Database(db) => db.message().to_string(),
            other => panic!("expected a refused statement, got {other:?}"),
        }
    }

    async fn admin_role_id(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT id FROM roles WHERE code = 'admin'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // -- AC13: the protected role itself is untouchable --------------------------
    // Each refusal is proved with the raw statement the interface (and any
    // future screen or script) would run, asserting the trigger's own error
    // text. The refusals are absolute: a second administrator changes nothing,
    // because they guard the role, not the count of its holders.

    #[tokio::test]
    async fn ac13_a_protected_role_cannot_be_deleted() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let err = sqlx::query("DELETE FROM roles WHERE code = 'admin'")
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "protected role cannot be deleted",
            "the trigger's own refusal text"
        );
        // A second administrator does not unlock it: the guard is absolute.
        seed_admin_holders(&p, &["second-admin"]).await;
        let err = sqlx::query("DELETE FROM roles WHERE code = 'admin'")
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(refusal_message(err), "protected role cannot be deleted");
    }

    #[tokio::test]
    async fn ac13_a_protected_role_code_cannot_change() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let err = sqlx::query("UPDATE roles SET code = 'root' WHERE code = 'admin'")
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(refusal_message(err), "protected role code cannot change");
    }

    #[tokio::test]
    async fn ac13_a_protected_role_permission_rows_cannot_be_removed() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let role_id = admin_role_id(&p).await;
        let err = sqlx::query("DELETE FROM role_permissions WHERE role_id = ?")
            .bind(role_id)
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "protected role permissions cannot be removed"
        );
    }

    /// The bypass the first verifier proved: with only the delete/rename
    /// guards, flipping the writable `is_system` column de-protected the role
    /// and two ordinary statements erased it. The flag is decided at seed
    /// time; flipping it either way is refused, so the delete guard cannot be
    /// disarmed by an UPDATE.
    #[tokio::test]
    async fn ac13_the_protected_status_flag_cannot_be_flipped() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        for sql in [
            "UPDATE roles SET is_system = 0 WHERE code = 'admin'",
            "UPDATE roles SET is_system = 1 WHERE code = 'vendedor'",
        ] {
            let err = sqlx::query(sql).execute(&p).await.unwrap_err();
            assert_eq!(
                refusal_message(err),
                "protected status is decided at seed time and cannot change",
                "flipping in either direction must abort: {sql}"
            );
        }
        // The attempted flip changed nothing: the role and its matrix survive.
        let roles = SqliteRoleRepository::new(p.clone());
        assert!(roles.find_by_code("admin").await.unwrap().unwrap().is_system);
        assert_eq!(
            roles.count_active_protected_holders().await.unwrap(),
            1,
            "an administrator still exists"
        );
    }

    /// The cascade origin of the AC14 arithmetic: deleting the user row would
    /// remove its grants while the user_roles trigger evaluates mid-cascade,
    /// so the deletion is guarded where it starts.
    #[tokio::test]
    async fn ac14_the_last_active_protected_holder_cannot_be_deleted_by_user_row() {
        let p = pool().await;
        // Granted by a persistent third user: a self-granted holder is also
        // held by the granted_by RESTRICT, which would mask the trigger.
        let grantor_user = SqliteUserRepository::new(p.clone())
            .create(
                &NewUser {
                    username: "hr-grantor".into(),
                    display_name: "HR".into(),
                    password_hash: "placeholder-not-a-real-argon2-hash".into(),
                    must_change_password: false,
                },
                None,
            )
            .await
            .unwrap();
        let holder = SqliteUserRepository::new(p.clone())
            .create(
                &NewUser {
                    username: "first-admin".into(),
                    display_name: "First".into(),
                    password_hash: "placeholder-not-a-real-argon2-hash".into(),
                    must_change_password: false,
                },
                Some(grantor_user.id),
            )
            .await
            .unwrap();
        let admin = SqliteRoleRepository::new(p.clone())
            .find_by_code("admin")
            .await
            .unwrap()
            .unwrap();
        SqliteRoleRepository::new(p.clone())
            .grant(&NewUserRole {
                user_id: holder.id,
                role_id: admin.id,
                granted_by: grantor_user.id,
            })
            .await
            .unwrap();

        let err = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(holder.id)
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "cannot delete the last active user holding a protected role"
        );
        // State intact.
        assert_eq!(
            SqliteRoleRepository::new(p.clone())
                .count_active_protected_holders()
                .await
                .unwrap(),
            1
        );
    }

    // -- AC14: the administrator arithmetic lives in the triggers -----------------

    #[tokio::test]
    async fn ac14_the_last_active_admin_cannot_be_deactivated_and_a_second_admin_unblocks() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let first: i64 =
            sqlx::query_scalar("SELECT id FROM users WHERE username = 'first-admin'")
                .fetch_one(&p)
                .await
                .unwrap();
        let err = sqlx::query("UPDATE users SET is_active = 0 WHERE id = ?")
            .bind(first)
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "cannot deactivate the last active user holding a protected role"
        );

        // With a second active administrator the same deactivation succeeds:
        // the trigger protects the last one, not every one.
        seed_admin_holders(&p, &["second-admin"]).await;
        let updated = sqlx::query("UPDATE users SET is_active = 0 WHERE id = ?")
            .bind(first)
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(updated.rows_affected(), 1, "the second admin unblocks it");
        // ... and the seed guard holds: one active protected-role holder left.
        let roles = SqliteRoleRepository::new(p.clone());
        assert_eq!(roles.count_active_protected_holders().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn ac14_the_last_grant_of_a_protected_role_cannot_be_deleted_and_a_second_admin_unblocks() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let role_id = admin_role_id(&p).await;
        let first: i64 =
            sqlx::query_scalar("SELECT id FROM users WHERE username = 'first-admin'")
                .fetch_one(&p)
                .await
                .unwrap();
        let err = sqlx::query("DELETE FROM user_roles WHERE user_id = ? AND role_id = ?")
            .bind(first)
            .bind(role_id)
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "cannot remove the last grant of a protected role to an active user"
        );

        // A second administrator makes the same deletion succeed.
        seed_admin_holders(&p, &["second-admin"]).await;
        let updated =
            sqlx::query("DELETE FROM user_roles WHERE user_id = ? AND role_id = ?")
                .bind(first)
                .bind(role_id)
                .execute(&p)
                .await
                .unwrap();
        assert_eq!(updated.rows_affected(), 1, "the second admin unblocks it");
        let roles = SqliteRoleRepository::new(p.clone());
        assert_eq!(roles.count_active_protected_holders().await.unwrap(), 1);
    }

    // -- the REPLACE hole (MINOR): closed by the shared connect options ----------
    // REPLACE-shaped statements fire BEFORE DELETE triggers only when
    // `PRAGMA recursive_triggers = ON`; the pragma is per connection, so every
    // pool that can write `roles`/`user_roles` takes its options from
    // `db::base_connect_options`. With it, both variants abort.

    #[tokio::test]
    async fn an_insert_or_replace_of_a_protected_role_row_aborts() {
        let p = pool().await;
        let err = sqlx::query(
            r#"INSERT OR REPLACE INTO roles (id, code, name, is_system, created_by)
               SELECT id, code, name, is_system, created_by FROM roles WHERE code = 'admin'"#,
        )
        .execute(&p)
        .await
        .unwrap_err();
        match err {
            sqlx::Error::Database(db) => assert_eq!(
                db.message(),
                "protected role cannot be deleted",
                "the REPLACE's delete phase must hit the guard trigger"
            ),
            other => panic!("expected the guard trigger, got {other:?}"),
        }
        // The row is untouched.
        let roles = SqliteRoleRepository::new(p.clone());
        let admin = roles.find_by_code("admin").await.unwrap().unwrap();
        assert!(admin.is_system);
        let (matrix_rows,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM role_permissions")
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(
            matrix_rows, 45,
            "the matrix survived the aborted REPLACE"
        );
    }

    #[tokio::test]
    async fn an_insert_or_replace_of_a_protected_grant_aborts() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let (user_id, role_id): (i64, i64) = sqlx::query_as(
            "SELECT ur.user_id, ur.role_id FROM user_roles ur
             JOIN roles r ON r.id = ur.role_id WHERE r.is_system = 1",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        let err = sqlx::query(
            r#"INSERT OR REPLACE INTO user_roles (user_id, role_id, granted_by)
               VALUES (?, ?, ?)"#,
        )
        .bind(user_id)
        .bind(role_id)
        .bind(user_id)
        .execute(&p)
        .await
        .unwrap_err();
        match err {
            sqlx::Error::Database(db) => assert_eq!(
                db.message(),
                "cannot remove the last grant of a protected role to an active user",
                "the REPLACE's delete phase must hit the grant guard"
            ),
            other => panic!("expected the guard trigger, got {other:?}"),
        }
    }

    // -- AC15: an assigned role is held by the RESTRICT foreign key ----------------

    #[tokio::test]
    async fn ac15_a_role_assigned_to_a_user_cannot_be_deleted() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let roles = SqliteRoleRepository::new(p.clone());
        let vendedor = roles.find_by_code("vendedor").await.unwrap().unwrap();
        let user: i64 =
            sqlx::query_scalar("SELECT id FROM users WHERE username = 'first-admin'")
                .fetch_one(&p)
                .await
                .unwrap();
        roles
            .grant(&NewUserRole {
                user_id: user,
                role_id: vendedor.id,
                granted_by: user,
            })
            .await
            .unwrap();
        let err = sqlx::query("DELETE FROM roles WHERE id = ?")
            .bind(vendedor.id)
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "FOREIGN KEY constraint failed",
            "user_roles.role_id is ON DELETE RESTRICT: assigned users block the delete"
        );
    }

    // -- S3 part 2: the delete path maps both refusals to Spanish conflicts ----

    /// The S4 screen will call `delete`; the mapped refusals it explains are
    /// shipped here so the message text cannot drift from the schema.
    #[tokio::test]
    async fn the_delete_refusal_for_a_protected_role_is_mapped_to_a_spanish_conflict() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let roles = SqliteRoleRepository::new(p.clone());
        let admin = roles.find_by_code("admin").await.unwrap().unwrap();
        let err = roles.delete(admin.id).await.unwrap_err();
        let msg = match &err {
            AppError::Conflict(m) => m.clone(),
            other => panic!("expected Conflict, got {other:?}"),
        };
        assert!(msg.contains("rol protegido"), "{msg}");
        assert!(!msg.contains("protected role"), "no raw trigger text: {msg}");
        assert!(roles.find_by_code("admin").await.unwrap().is_some());
    }

    /// AC15's interface explanation: a role held by users cannot be deleted,
    /// and the refusal names the reason in Spanish (the S4 screen names the
    /// blocking users; the message exists here first).
    #[tokio::test]
    async fn the_delete_refusal_for_an_assigned_role_names_the_users_that_block_it() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let roles = SqliteRoleRepository::new(p.clone());
        let vendedor = roles.find_by_code("vendedor").await.unwrap().unwrap();
        let user: i64 =
            sqlx::query_scalar("SELECT id FROM users WHERE username = 'first-admin'")
                .fetch_one(&p)
                .await
                .unwrap();
        roles
            .grant(&NewUserRole {
                user_id: user,
                role_id: vendedor.id,
                granted_by: user,
            })
            .await
            .unwrap();
        let err = roles.delete(vendedor.id).await.unwrap_err();
        let msg = match &err {
            AppError::Conflict(m) => m.clone(),
            other => panic!("expected Conflict, got {other:?}"),
        };
        assert!(msg.contains("usuarios"), "{msg}");
        assert!(msg.contains("asignado"), "{msg}");
        assert!(!msg.contains("FOREIGN KEY"), "no raw SQL text: {msg}");
        assert!(roles.find_by_code("vendedor").await.unwrap().is_some());
    }

    /// A role nobody holds deletes cleanly through the same path.
    #[tokio::test]
    async fn an_unassigned_role_deletes_cleanly() {
        let p = pool().await;
        let roles = SqliteRoleRepository::new(p.clone());
        let vendedor = roles.find_by_code("vendedor").await.unwrap().unwrap();
        roles.delete(vendedor.id).await.unwrap();
        assert!(roles.find_by_code("vendedor").await.unwrap().is_none());
    }

    /// `holder_names` has NO `is_active` filter, on purpose: the `user_roles`
    /// RESTRICT foreign key does not distinguish active from inactive
    /// holders, so neither does the read that names them (spec: "the refusal
    /// names the blocking users, whatever their state"). A DEACTIVATED
    /// holder must be named too — otherwise the operator would face a
    /// deletion refusal naming nobody — and the deletion stays refused with
    /// the deactivated holder still in the set.
    #[tokio::test]
    async fn holder_names_names_a_deactivated_holder_too_and_the_delete_stays_refused() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin", "second-admin"]).await;
        let users = SqliteUserRepository::new(p.clone());
        let roles = SqliteRoleRepository::new(p.clone());
        let vendedor = roles.find_by_code("vendedor").await.unwrap().unwrap();
        let first: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'first-admin'")
            .fetch_one(&p)
            .await
            .unwrap();
        let second: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'second-admin'")
            .fetch_one(&p)
            .await
            .unwrap();
        for user_id in [first, second] {
            roles
                .grant(&NewUserRole { user_id, role_id: vendedor.id, granted_by: user_id })
                .await
                .unwrap();
        }
        users.set_active(first, false, None).await.unwrap();

        // The read is the TOTAL set: the deactivated holder is named too.
        let named = roles.holder_names(vendedor.id).await.unwrap();
        assert_eq!(named, vec!["first-admin".to_string(), "second-admin".to_string()]);

        // And the deletion is still refused with the holders' reason.
        let err = roles.delete(vendedor.id).await.unwrap_err();
        match &err {
            AppError::Conflict(m) => {
                assert!(m.contains("asignado"), "{m}");
                assert!(!m.contains("FOREIGN KEY"), "no raw SQL text: {m}");
            }
            other => panic!("expected the holders conflict, got {other:?}"),
        }
        assert!(roles.find_by_code("vendedor").await.unwrap().is_some());
    }

    // -- the shared mapper knows the two delete-shaped refusals (correction
    //    round: the protected delete refusal fell through to a generic 500
    //    on any path but `delete`, and the FK branch claimed the grant
    //    context's reason for a refusal it cannot know) -------------------

    /// The protected-delete trigger mapped by the SHARED mapper, not only by
    /// `delete`'s own closure: a future path that deletes a role row through
    /// `map_db_err` reports the Spanish conflict instead of a raw 500.
    #[tokio::test]
    async fn map_db_err_maps_the_protected_delete_trigger_to_a_spanish_conflict() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let err = sqlx::query("DELETE FROM roles WHERE code = 'admin'")
            .execute(&p)
            .await
            .unwrap_err();
        match map_db_err(err) {
            AppError::Conflict(m) => {
                assert!(m.contains("rol protegido"), "{m}");
                assert!(!m.contains("protected role"), "no raw trigger text: {m}");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    /// The FK branch keeps its grant-context claim: it is reached by the
    /// grant/replace statements (a submitted id naming a missing row), and
    /// the delete path maps the holders' refusal itself.
    #[tokio::test]
    async fn a_grant_naming_a_missing_role_is_a_validation_naming_the_submitted_set() {
        let p = pool().await;
        seed_admin_holders(&p, &["first-admin"]).await;
        let user: i64 =
            sqlx::query_scalar("SELECT id FROM users WHERE username = 'first-admin'")
                .fetch_one(&p)
                .await
                .unwrap();
        let err = SqliteRoleRepository::new(p.clone())
            .grant(&NewUserRole {
                user_id: user,
                role_id: 999_999,
                granted_by: user,
            })
            .await
            .unwrap_err();
        match err {
            AppError::Validation(m) => assert!(m.contains("roles indicados"), "{m}"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    // -- S3 correction round: the assignment set is resolved in one query ----

    #[tokio::test]
    async fn find_by_ids_answers_the_existing_subset_of_many_ids_in_one_statement() {
        let p = pool().await;
        let roles = SqliteRoleRepository::new(p.clone());
        let vendedor = roles.find_by_code("vendedor").await.unwrap().unwrap();
        let cajero = roles.find_by_code("cajero").await.unwrap().unwrap();
        // Duplicates in, existing ids mixed with a missing one: the answer is
        // the existing subset, in the statement's own order.
        let found = roles
            .find_by_ids(&[vendedor.id, 999_999, cajero.id, vendedor.id])
            .await
            .unwrap();
        let mut codes: Vec<&str> = found.iter().map(|r| r.code.as_str()).collect();
        codes.sort();
        assert_eq!(codes, vec!["cajero", "vendedor"]);
        // The empty set asks nothing.
        assert!(roles.find_by_ids(&[]).await.unwrap().is_empty());
    }

    // -- S13: the grant trail read ------------------------------------------------

    /// `list_grants_for_user` reads the `user_roles` columns the RBAC slice
    /// has recorded since S2: one entry per held role, the granter's id and
    /// the recorded instant, in the same role order as `list_for_user`. The
    /// display name is the wiring layer's job; the repository returns the
    /// stored ids, never resolved names.
    #[tokio::test]
    async fn list_grants_for_user_returns_the_recorded_grant_trail_in_role_order() {
        let p = pool().await;
        let users = SqliteUserRepository::new(p.clone());
        let roles = SqliteRoleRepository::new(p.clone());
        let granter = users
            .create(
                &NewUser {
                    username: "grant-giver".into(),
                    display_name: "Grant Giver".into(),
                    password_hash: "placeholder-not-a-real-argon2-hash".into(),
                    must_change_password: false,
                },
                None,
            )
            .await
            .unwrap();
        let target = users
            .create(
                &NewUser {
                    username: "teller".into(),
                    display_name: "Teller".into(),
                    password_hash: "placeholder-not-a-real-argon2-hash".into(),
                    must_change_password: false,
                },
                Some(granter.id),
            )
            .await
            .unwrap();
        let vendedor = roles.find_by_code("vendedor").await.unwrap().unwrap();
        let cajero = roles.find_by_code("cajero").await.unwrap().unwrap();
        roles
            .grant(&NewUserRole {
                user_id: target.id,
                role_id: vendedor.id,
                granted_by: granter.id,
            })
            .await
            .unwrap();
        roles
            .replace_user_roles(target.id, &[vendedor.id, cajero.id], granter.id)
            .await
            .unwrap();

        let trail = roles.list_grants_for_user(target.id).await.unwrap();
        assert_eq!(
            trail
                .iter()
                .map(|g| g.role.code.as_str())
                .collect::<Vec<_>>(),
            vec!["vendedor", "cajero"],
            "the trail follows the same role order as list_for_user"
        );
        for grant in &trail {
            assert_eq!(grant.granted_by, granter.id, "the granter is recorded");
            assert!(!grant.granted_at.to_string().is_empty(), "the instant is recorded");
        }
        // A user holding nothing answers the empty trail, never an error.
        assert!(roles.list_grants_for_user(granter.id).await.unwrap().is_empty());
    }
}
