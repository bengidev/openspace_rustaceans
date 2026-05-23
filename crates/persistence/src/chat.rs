//! SQLite-backed chat repository.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use openspace_shared::ai::domain::{Conversation, Turn};
use openspace_shared::id::{ChatId, TurnId, WorkspaceId};
use openspace_shared::persistence::{ChatRepository, PersistenceError};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::sql::{conflict, id_to_string, map_sql_error, not_found};
use crate::Database;

/// SQLite implementation of [`ChatRepository`].
#[derive(Debug, Clone)]
pub struct SqliteChatRepository {
    db: Database,
}

impl SqliteChatRepository {
    /// Build a repository over an opened database handle.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ChatRepository for SqliteChatRepository {
    async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        chat: &Conversation,
    ) -> Result<(), PersistenceError> {
        let row = ChatRow::from_chat(workspace_id, chat);
        self.db
            .connection()
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO chats (id, workspace_id, title, model_provider, model_id, system_prompt, head_turn_id, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8)
                     ON CONFLICT(id) DO UPDATE SET
                        workspace_id = excluded.workspace_id,
                        title = excluded.title,
                        system_prompt = excluded.system_prompt,
                        updated_at = excluded.updated_at",
                    params![
                        row.id,
                        row.workspace_id,
                        row.title,
                        "",
                        "",
                        row.system_prompt,
                        row.created_at,
                        row.updated_at
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn get(&self, id: ChatId) -> Result<Option<Conversation>, PersistenceError> {
        let id = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, title, system_prompt FROM chats WHERE id = ?1",
                        [id],
                        chat_from_row,
                    )
                    .optional()?)
            })
            .await
            .map_err(map_sql_error)
    }

    async fn delete(&self, id: ChatId) -> Result<(), PersistenceError> {
        let id = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                let changed = conn.execute("DELETE FROM chats WHERE id = ?1", [id.clone()])?;
                if changed == 0 {
                    return Err(not_found(format!("chat/{id}")));
                }
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Conversation>, PersistenceError> {
        let workspace_id = id_to_string!(workspace_id);
        self.db
            .connection()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, title, system_prompt FROM chats WHERE workspace_id = ?1 ORDER BY updated_at DESC",
                )?;
                let rows = stmt.query_map([workspace_id], chat_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(map_sql_error)
    }

    async fn set_head_turn(
        &self,
        chat_id: ChatId,
        turn_id: Option<TurnId>,
    ) -> Result<(), PersistenceError> {
        let chat_id = id_to_string!(chat_id);
        let turn_id = turn_id.map(|id| id_to_string!(id));
        self.db
            .connection()
            .call(move |conn| {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM chats WHERE id = ?1)",
                    [chat_id.clone()],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Err(not_found(format!("chat/{chat_id}")));
                }
                if let Some(turn_id) = &turn_id {
                    let owner: Option<String> = conn
                        .query_row(
                            "SELECT chat_id FROM turns WHERE id = ?1",
                            [turn_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match owner {
                        Some(owner) if owner == chat_id => {}
                        Some(_) => {
                            return Err(conflict(format!("turn/{turn_id} belongs to another chat")))
                        }
                        None => return Err(not_found(format!("turn/{turn_id}"))),
                    }
                }
                conn.execute(
                    "UPDATE chats SET head_turn_id = ?2, updated_at = ?3 WHERE id = ?1",
                    params![chat_id, turn_id, Utc::now().timestamp()],
                )?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn search(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
    ) -> Result<Vec<ChatId>, PersistenceError> {
        let workspace_id = id_to_string!(workspace_id);
        let query = format!("%{query}%");
        self.db
            .connection()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id FROM chats WHERE workspace_id = ?1 AND title LIKE ?2 ORDER BY updated_at DESC",
                )?;
                let rows = stmt.query_map(params![workspace_id, query], |row| {
                    let id: String = row.get(0)?;
                    Ok(ChatId::from_uuid(Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::nil())))
                })?
                .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(map_sql_error)
    }
}

struct ChatRow {
    id: String,
    workspace_id: String,
    title: String,
    system_prompt: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl ChatRow {
    fn from_chat(workspace_id: WorkspaceId, chat: &Conversation) -> Self {
        let updated_at = chat.turns.last().map_or_else(
            || Utc::now().timestamp(),
            |turn| turn.created_at.timestamp(),
        );
        Self {
            id: id_to_string!(chat.id),
            workspace_id: id_to_string!(workspace_id),
            title: chat.title.clone().unwrap_or_default(),
            system_prompt: chat.system_prompt.clone(),
            created_at: DateTime::<Utc>::UNIX_EPOCH.timestamp(),
            updated_at,
        }
    }
}

fn chat_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    let id: String = row.get(0)?;
    let title: String = row.get(1)?;
    let system_prompt: Option<String> = row.get(2)?;
    Ok(Conversation::new(
        ChatId::from_uuid(Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::nil())),
        if title.is_empty() { None } else { Some(title) },
        Vec::<Turn>::new(),
        system_prompt,
    ))
}
