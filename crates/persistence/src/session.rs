//! SQLite-backed session repository.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use openspace_shared::id::{SessionId, WorkspaceId};
use openspace_shared::persistence::types::{Session, SessionMode};
use openspace_shared::persistence::{PersistenceError, SessionRepository};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::sql::{id_to_string, map_sql_error, not_found, ts_to_datetime, write_transaction};
use crate::Database;

/// SQLite implementation of [`SessionRepository`].
#[derive(Debug, Clone)]
pub struct SqliteSessionRepository {
    db: Database,
}

impl SqliteSessionRepository {
    /// Build a repository over an opened database handle.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Alias for issue #71 wording; forwards to [`SessionRepository::list_for_workspace`].
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Session>, PersistenceError> {
        self.list_for_workspace(workspace_id, None).await
    }
}

#[async_trait]
impl SessionRepository for SqliteSessionRepository {
    async fn upsert(&self, session: &Session) -> Result<(), PersistenceError> {
        let row = SessionRow::from_session(session);
        write_transaction(&self.db, move |tx| {
                tx.execute(
                    "INSERT INTO sessions (id, workspace_id, name, mode, layout_blob, active_pane_id, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)
                     ON CONFLICT(id) DO UPDATE SET
                        workspace_id = excluded.workspace_id,
                        name = excluded.name,
                        mode = excluded.mode,
                        updated_at = excluded.updated_at",
                    params![row.id, row.workspace_id, row.title, row.mode, "", row.updated_at],
                )?;
                Ok(())
        })
        .await
    }

    async fn get(&self, id: SessionId) -> Result<Option<Session>, PersistenceError> {
        let id = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, workspace_id, name, mode, updated_at FROM sessions WHERE id = ?1",
                        [id],
                        session_from_row,
                    )
                    .optional()?)
            })
            .await
            .map_err(map_sql_error)
    }

    async fn delete(&self, id: SessionId) -> Result<(), PersistenceError> {
        let id = id_to_string!(id);
        write_transaction(&self.db, move |tx| {
            let changed = tx.execute("DELETE FROM sessions WHERE id = ?1", [id.clone()])?;
            if changed == 0 {
                return Err(not_found(format!("session/{id}")));
            }
            Ok(())
        })
        .await
    }

    async fn list_for_workspace(
        &self,
        workspace_id: WorkspaceId,
        mode: Option<SessionMode>,
    ) -> Result<Vec<Session>, PersistenceError> {
        let workspace_id = id_to_string!(workspace_id);
        let mode = mode.map(mode_to_str);
        self.db
            .connection()
            .call(move |conn| {
                if let Some(mode) = mode {
                    let mut stmt = conn.prepare(
                        "SELECT id, workspace_id, name, mode, updated_at
                         FROM sessions
                         WHERE workspace_id = ?1 AND mode = ?2
                         ORDER BY updated_at DESC",
                    )?;
                    let rows = stmt
                        .query_map(params![workspace_id, mode], session_from_row)?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                } else {
                    let mut stmt = conn.prepare(
                        "SELECT id, workspace_id, name, mode, updated_at
                         FROM sessions
                         WHERE workspace_id = ?1
                         ORDER BY updated_at DESC",
                    )?;
                    let rows = stmt
                        .query_map([workspace_id], session_from_row)?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                }
            })
            .await
            .map_err(map_sql_error)
    }
}

struct SessionRow {
    id: String,
    workspace_id: String,
    title: String,
    mode: &'static str,
    updated_at: i64,
}

impl SessionRow {
    fn from_session(session: &Session) -> Self {
        Self {
            id: id_to_string!(session.id),
            workspace_id: id_to_string!(session.workspace_id),
            title: session.title.clone().unwrap_or_default(),
            mode: mode_to_str(session.mode),
            updated_at: session.updated_at.timestamp(),
        }
    }
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let id: String = row.get(0)?;
    let workspace_id: String = row.get(1)?;
    let title: String = row.get(2)?;
    let mode: String = row.get(3)?;
    let updated_at: i64 = row.get(4)?;
    let updated_at = ts_to_datetime(updated_at)?;
    Ok(Session::new(
        SessionId::from_uuid(Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::nil())),
        WorkspaceId::from_uuid(Uuid::parse_str(&workspace_id).unwrap_or_else(|_| Uuid::nil())),
        mode_from_str(&mode),
        if title.is_empty() { None } else { Some(title) },
        DateTime::<Utc>::UNIX_EPOCH,
        updated_at,
    ))
}

fn mode_to_str(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Terminal => "terminal",
        SessionMode::Chat => "chat",
        SessionMode::Editor => "editor",
    }
}

fn mode_from_str(mode: &str) -> SessionMode {
    match mode {
        "terminal" => SessionMode::Terminal,
        "editor" => SessionMode::Editor,
        _ => SessionMode::Chat,
    }
}
