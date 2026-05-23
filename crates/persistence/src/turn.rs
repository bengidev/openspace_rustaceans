//! SQLite-backed turn repository.

use async_trait::async_trait;
use openspace_shared::ai::domain::Turn;
use openspace_shared::id::{ChatId, TurnId};
use openspace_shared::persistence::{PersistenceError, TurnRepository};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::sql::{id_to_string, map_sql_error, ts_to_datetime, write_transaction};
use crate::Database;

/// SQLite implementation of [`TurnRepository`].
#[derive(Debug, Clone)]
pub struct SqliteTurnRepository {
    db: Database,
}

impl SqliteTurnRepository {
    /// Build a repository over an opened database handle.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Issue #72 alias; forwards to [`TurnRepository::list_for_chat`].
    pub async fn list_by_chat(&self, chat_id: ChatId) -> Result<Vec<Turn>, PersistenceError> {
        self.list_for_chat(chat_id).await
    }

    /// Append a turn with an explicit parent pointer.
    pub async fn append_with_parent(
        &self,
        chat_id: ChatId,
        parent_turn_id: Option<TurnId>,
        turn: &Turn,
    ) -> Result<(), PersistenceError> {
        let row = TurnRow::from_turn(chat_id, parent_turn_id, turn)?;
        write_transaction(&self.db, move |tx| {
            let sequence: i64 = tx.query_row(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM turns WHERE chat_id = ?1",
                [&row.chat_id],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO turns (id, chat_id, parent_turn_id, kind, payload_json, sequence, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.id,
                    row.chat_id,
                    row.parent_turn_id,
                    row.kind,
                    row.payload_json,
                    sequence,
                    row.created_at
                ],
            )?;
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl TurnRepository for SqliteTurnRepository {
    async fn append(&self, chat_id: ChatId, turn: &Turn) -> Result<(), PersistenceError> {
        self.append_with_parent(chat_id, None, turn).await
    }

    async fn get(&self, id: TurnId) -> Result<Option<Turn>, PersistenceError> {
        let id = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, kind, payload_json, created_at FROM turns WHERE id = ?1",
                        [id],
                        turn_from_row,
                    )
                    .optional()?)
            })
            .await
            .map_err(map_sql_error)
    }

    async fn list_for_chat(&self, chat_id: ChatId) -> Result<Vec<Turn>, PersistenceError> {
        let chat_id = id_to_string!(chat_id);
        self.db
            .connection()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, kind, payload_json, created_at FROM turns WHERE chat_id = ?1 ORDER BY sequence ASC",
                )?;
                let rows = stmt
                    .query_map([chat_id], turn_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(map_sql_error)
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(TurnId, ChatId)>, PersistenceError> {
        let query = query.to_string();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.db
            .connection()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT turn_id, chat_id
                     FROM turn_text_fts
                     WHERE turn_text_fts MATCH ?1
                     ORDER BY rank
                     LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(params![query, limit], search_hit_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(map_sql_error)
    }
}

struct TurnRow {
    id: String,
    chat_id: String,
    parent_turn_id: Option<String>,
    kind: String,
    payload_json: String,
    created_at: i64,
}

impl TurnRow {
    fn from_turn(
        chat_id: ChatId,
        parent_turn_id: Option<TurnId>,
        turn: &Turn,
    ) -> Result<Self, PersistenceError> {
        Ok(Self {
            id: id_to_string!(turn.id),
            chat_id: id_to_string!(chat_id),
            parent_turn_id: parent_turn_id.map(|id| id_to_string!(id)),
            kind: serde_json::to_string(&turn.role)
                .map_err(|err| PersistenceError::IoError(err.to_string()))?,
            payload_json: serde_json::to_string(turn)
                .map_err(|err| PersistenceError::IoError(err.to_string()))?,
            created_at: turn.created_at.timestamp(),
        })
    }
}

fn turn_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
    let id: String = row.get(0)?;
    let payload_json: String = row.get(2)?;
    let mut turn: Turn = serde_json::from_str(&payload_json).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err))
    })?;
    turn.id = TurnId::from_uuid(Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::nil()));
    let created_at: i64 = row.get(3)?;
    turn.created_at = ts_to_datetime(created_at)?;
    Ok(turn)
}

fn search_hit_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(TurnId, ChatId)> {
    let turn_id: String = row.get(0)?;
    let chat_id: String = row.get(1)?;
    Ok((
        TurnId::from_uuid(Uuid::parse_str(&turn_id).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
        })?),
        ChatId::from_uuid(Uuid::parse_str(&chat_id).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(err))
        })?),
    ))
}
