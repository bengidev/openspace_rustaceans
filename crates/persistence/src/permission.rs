//! SQLite-backed permission grant store.

use async_trait::async_trait;
use openspace_shared::id::{PermissionGrantId, WorkspaceId};
use openspace_shared::persistence::types::{PermissionDecision, PermissionGrant, PermissionScope};
use openspace_shared::persistence::{PermissionGrantStore, PersistenceError};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::sql::{id_to_string, map_sql_error, ts_to_datetime};
use crate::Database;

/// SQLite implementation of [`PermissionGrantStore`].
#[derive(Debug, Clone)]
pub struct SqlitePermissionGrantStore {
    db: Database,
}

impl SqlitePermissionGrantStore {
    /// Build a store over an opened database handle.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl PermissionGrantStore for SqlitePermissionGrantStore {
    async fn upsert(&self, grant: &PermissionGrant) -> Result<(), PersistenceError> {
        let row = PermissionGrantRow::from_grant(grant)?;
        self.db
            .connection()
            .call(move |conn| {
                // The schema keys grants by workspace/tool/pattern. Re-recording the same
                // tuple refreshes `granted_at`; it never creates a duplicate row.
                conn.execute(
                    "INSERT INTO permission_grants (workspace_id, tool_id, network_pattern, granted_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(workspace_id, tool_id, network_pattern) DO UPDATE SET
                        granted_at = excluded.granted_at",
                    params![row.workspace_id, row.tool_id, row.network_pattern, row.granted_at],
                )?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn get(
        &self,
        id: PermissionGrantId,
    ) -> Result<Option<PermissionGrant>, PersistenceError> {
        let network_pattern = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT workspace_id, tool_id, network_pattern, granted_at
                     FROM permission_grants
                     WHERE network_pattern = ?1
                     ORDER BY granted_at DESC
                     LIMIT 1",
                        [network_pattern],
                        permission_grant_from_row,
                    )
                    .optional()?)
            })
            .await
            .map_err(map_sql_error)
    }

    async fn delete(&self, id: PermissionGrantId) -> Result<(), PersistenceError> {
        let network_pattern = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM permission_grants WHERE network_pattern = ?1",
                    [network_pattern],
                )?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn list_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<PermissionGrant>, PersistenceError> {
        let workspace_id = id_to_string!(workspace_id);
        self.db
            .connection()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT workspace_id, tool_id, network_pattern, granted_at
                     FROM permission_grants
                     WHERE workspace_id = ?1
                     ORDER BY granted_at DESC",
                )?;
                let rows = stmt
                    .query_map([workspace_id], permission_grant_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(map_sql_error)
    }
}

struct PermissionGrantRow {
    workspace_id: String,
    tool_id: String,
    network_pattern: String,
    granted_at: i64,
}

impl PermissionGrantRow {
    fn from_grant(grant: &PermissionGrant) -> Result<Self, PersistenceError> {
        let network_pattern = id_to_string!(grant.id);
        Ok(Self {
            workspace_id: id_to_string!(grant.workspace_id),
            tool_id: grant.tool_id.clone(),
            network_pattern,
            granted_at: grant.created_at.timestamp(),
        })
    }
}

fn permission_grant_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PermissionGrant> {
    let workspace_id: String = row.get(0)?;
    let tool_id: String = row.get(1)?;
    let network_pattern: String = row.get(2)?;
    let granted_at: i64 = row.get(3)?;
    Ok(PermissionGrant::new(
        PermissionGrantId::from_uuid(
            Uuid::parse_str(&network_pattern).unwrap_or_else(|_| Uuid::nil()),
        ),
        WorkspaceId::from_uuid(Uuid::parse_str(&workspace_id).unwrap_or_else(|_| Uuid::nil())),
        None,
        tool_id,
        PermissionScope::Workspace,
        PermissionDecision::Allow,
        None,
        ts_to_datetime(granted_at)?,
    ))
}
