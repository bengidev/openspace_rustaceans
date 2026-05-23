//! SQLite-backed workspace repositories.

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::Utc;
use openspace_shared::id::WorkspaceId;
use openspace_shared::persistence::{
    PersistenceError, RecentWorkspacesRepository, WorkspaceRepository,
};
use openspace_shared::workspace::{Workspace, WorkspaceRef};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::Database;

/// SQLite implementation of [`WorkspaceRepository`].
#[derive(Debug, Clone)]
pub struct SqliteWorkspaceRepository {
    db: Database,
}

impl SqliteWorkspaceRepository {
    /// Build a repository over an opened database handle.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl WorkspaceRepository for SqliteWorkspaceRepository {
    async fn upsert(&self, workspace: &Workspace) -> Result<(), PersistenceError> {
        let row = WorkspaceRow::from_workspace(workspace);
        self.db
            .connection()
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO workspaces (id, root_path, last_opened_at, trust_mode, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(id) DO UPDATE SET
                        root_path = excluded.root_path,
                        trust_mode = excluded.trust_mode",
                    params![
                        row.id,
                        row.root_path,
                        row.last_opened_at,
                        row.trust_mode,
                        row.created_at
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn get(&self, id: WorkspaceId) -> Result<Option<Workspace>, PersistenceError> {
        let id = id_to_string(id);
        self.db
            .connection()
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, root_path, trust_mode FROM workspaces WHERE id = ?1",
                        [id],
                        workspace_from_row,
                    )
                    .optional()?)
            })
            .await
            .map_err(map_sql_error)
    }

    async fn delete(&self, id: WorkspaceId) -> Result<(), PersistenceError> {
        let id = id_to_string(id);
        self.db
            .connection()
            .call(move |conn| {
                let has_sessions: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE workspace_id = ?1)",
                    [id.clone()],
                    |row| row.get(0),
                )?;
                if has_sessions {
                    return Err(tokio_rusqlite::Error::Rusqlite(
                        rusqlite::Error::InvalidParameterName(format!(
                            "conflict: workspace/{id} owns sessions"
                        )),
                    ));
                }
                let changed = conn.execute("DELETE FROM workspaces WHERE id = ?1", [id.clone()])?;
                if changed == 0 {
                    return Err(tokio_rusqlite::Error::Rusqlite(
                        rusqlite::Error::InvalidParameterName(format!("not found: workspace/{id}")),
                    ));
                }
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn list(&self) -> Result<Vec<Workspace>, PersistenceError> {
        self.db
            .connection()
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, root_path, trust_mode FROM workspaces ORDER BY trust_mode ASC",
                )?;
                let rows = stmt
                    .query_map([], workspace_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(map_sql_error)
    }
}

/// SQLite implementation of [`RecentWorkspacesRepository`].
#[derive(Debug, Clone)]
pub struct SqliteRecentWorkspacesRepository {
    db: Database,
}

impl SqliteRecentWorkspacesRepository {
    /// Build a repository over an opened database handle.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl RecentWorkspacesRepository for SqliteRecentWorkspacesRepository {
    async fn record_opened(&self, id: WorkspaceId) -> Result<(), PersistenceError> {
        let id = id_to_string(id);
        let now = Utc::now().timestamp();
        self.db
            .connection()
            .call(move |conn| {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id = ?1)",
                    [id.clone()],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Err(tokio_rusqlite::Error::Rusqlite(
                        rusqlite::Error::InvalidParameterName(format!("not found: workspace/{id}")),
                    ));
                }
                conn.execute(
                    "INSERT INTO recent_workspaces (workspace_id, last_opened_at)
                     VALUES (?1, ?2)
                     ON CONFLICT(workspace_id) DO UPDATE SET
                        last_opened_at = excluded.last_opened_at",
                    params![id, now],
                )?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn forget(&self, id: WorkspaceId) -> Result<(), PersistenceError> {
        let id = id_to_string(id);
        self.db
            .connection()
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM recent_workspaces WHERE workspace_id = ?1",
                    [id],
                )?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn list(&self, limit: Option<usize>) -> Result<Vec<WorkspaceRef>, PersistenceError> {
        let limit = limit.map(i64::try_from).transpose().map_err(|_| {
            PersistenceError::ConflictingWrite("recent workspace limit exceeds i64".into())
        })?;
        self.db
            .connection()
            .call(move |conn| {
                let sql = "SELECT w.id, w.root_path, w.trust_mode
                           FROM recent_workspaces rw
                           JOIN workspaces w ON w.id = rw.workspace_id
                           ORDER BY rw.last_opened_at DESC";
                let limited = format!("{sql} LIMIT ?1");
                if let Some(limit) = limit {
                    let mut stmt = conn.prepare(&limited)?;
                    let rows = stmt
                        .query_map([limit], workspace_ref_from_row)?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                } else {
                    let mut stmt = conn.prepare(sql)?;
                    let rows = stmt
                        .query_map([], workspace_ref_from_row)?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                }
            })
            .await
            .map_err(map_sql_error)
    }
}

struct WorkspaceRow {
    id: String,
    root_path: String,
    last_opened_at: i64,
    trust_mode: String,
    created_at: i64,
}

impl WorkspaceRow {
    fn from_workspace(workspace: &Workspace) -> Self {
        let now = Utc::now().timestamp();
        Self {
            id: id_to_string(workspace.id),
            root_path: workspace.root.to_string_lossy().into_owned(),
            last_opened_at: now,
            trust_mode: workspace.display_name.clone(),
            created_at: now,
        }
    }
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
    let id: String = row.get(0)?;
    let root_path: String = row.get(1)?;
    let display_name: String = row.get(2)?;
    Ok(workspace_from_parts(&id, root_path, display_name))
}

fn workspace_ref_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRef> {
    let id: String = row.get(0)?;
    let root_path: String = row.get(1)?;
    let display_name: String = row.get(2)?;
    Ok(workspace_from_parts(&id, root_path, display_name).as_ref())
}

fn workspace_from_parts(id: &str, root_path: String, display_name: String) -> Workspace {
    let root = PathBuf::from(root_path);
    let uuid = Uuid::parse_str(id).unwrap_or_else(|_| Uuid::nil());
    Workspace::new(WorkspaceId::from_uuid(uuid), root, display_name)
}

fn id_to_string(id: WorkspaceId) -> String {
    id.as_uuid().to_string()
}

fn map_sql_error(err: tokio_rusqlite::Error) -> PersistenceError {
    match &err {
        tokio_rusqlite::Error::Rusqlite(rusqlite::Error::InvalidParameterName(message))
            if message.starts_with("not found:") =>
        {
            PersistenceError::NotFound(message.trim_start_matches("not found: ").to_string())
        }
        tokio_rusqlite::Error::Rusqlite(rusqlite::Error::InvalidParameterName(message))
            if message.starts_with("conflict:") =>
        {
            PersistenceError::ConflictingWrite(message.trim_start_matches("conflict: ").to_string())
        }
        tokio_rusqlite::Error::Rusqlite(rusqlite::Error::SqliteFailure(code, detail)) => {
            let detail = detail
                .as_ref()
                .map_or_else(|| err.to_string(), ToString::to_string);
            match code.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    PersistenceError::DatabaseLocked(detail)
                }
                rusqlite::ErrorCode::ConstraintViolation => {
                    PersistenceError::ConflictingWrite(detail)
                }
                _ => PersistenceError::IoError(detail),
            }
        }
        _ => PersistenceError::IoError(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openspace_shared::id::SessionId;
    use openspace_shared::persistence::types::{Session, SessionMode};
    use openspace_shared::persistence::{RecentWorkspacesRepository, WorkspaceRepository};

    fn migrations_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")
    }

    async fn repos() -> (
        Database,
        SqliteWorkspaceRepository,
        SqliteRecentWorkspacesRepository,
    ) {
        let db = Database::open_in_memory(migrations_dir())
            .await
            .expect("db");
        (
            db.clone(),
            SqliteWorkspaceRepository::new(db.clone()),
            SqliteRecentWorkspacesRepository::new(db),
        )
    }

    fn workspace(name: &str) -> Workspace {
        Workspace::new(
            WorkspaceId::new_v4(),
            PathBuf::from(format!("/tmp/{name}")),
            name.to_string(),
        )
    }

    #[tokio::test]
    async fn workspace_round_trip() {
        let (_db, repo, _recent) = repos().await;
        let original = workspace("alpha");
        repo.upsert(&original).await.expect("insert");
        assert_eq!(
            repo.get(original.id).await.expect("get"),
            Some(original.clone())
        );

        let updated = Workspace::new(original.id, PathBuf::from("/tmp/beta"), "beta".into());
        repo.upsert(&updated).await.expect("update");
        assert_eq!(
            repo.get(original.id).await.expect("get updated"),
            Some(updated.clone())
        );
        assert_eq!(repo.list().await.expect("list"), vec![updated.clone()]);

        repo.delete(updated.id).await.expect("delete");
        assert!(repo.list().await.expect("empty").is_empty());
    }

    #[tokio::test]
    async fn recents_list_most_recent_first() {
        let (_db, repo, recent) = repos().await;
        let first = workspace("first");
        let second = workspace("second");
        repo.upsert(&first).await.expect("insert first");
        repo.upsert(&second).await.expect("insert second");

        recent.record_opened(first.id).await.expect("first recent");
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        recent
            .record_opened(second.id)
            .await
            .expect("second recent");

        let listed = recent.list(None).await.expect("list recent");
        assert_eq!(listed, vec![second.as_ref(), first.as_ref()]);
        assert_eq!(
            recent.list(Some(1)).await.expect("limited"),
            vec![second.as_ref()]
        );
    }

    #[tokio::test]
    async fn missing_workspace_errors_are_not_found() {
        let (_db, repo, recent) = repos().await;
        let missing = WorkspaceId::new_v4();
        assert_eq!(repo.get(missing).await.expect("get missing"), None);
        assert!(matches!(
            repo.delete(missing).await,
            Err(PersistenceError::NotFound(_))
        ));
        assert!(matches!(
            recent.record_opened(missing).await,
            Err(PersistenceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn deleting_workspace_with_session_conflicts() {
        let (db, repo, _recent) = repos().await;
        let workspace = workspace("owned");
        repo.upsert(&workspace).await.expect("insert workspace");
        let session = Session::new(
            SessionId::new_v4(),
            workspace.id,
            SessionMode::Chat,
            Some("chat".into()),
            Utc::now(),
            Utc::now(),
        );
        let sid = session.id.as_uuid().to_string();
        let wid = workspace.id.as_uuid().to_string();
        db.connection()
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO sessions (id, workspace_id, name, mode, layout_blob, active_pane_id, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
                    params![sid, wid, "chat", "chat", "{}", Utc::now().timestamp()],
                )?;
                Ok(())
            })
            .await
            .expect("insert session");

        assert!(matches!(
            repo.delete(workspace.id).await,
            Err(PersistenceError::ConflictingWrite(_))
        ));
    }
}
