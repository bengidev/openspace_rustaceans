//! SQLite-backed attachment store.

use async_trait::async_trait;
use openspace_shared::id::{AttachmentId, ChatId};
use openspace_shared::persistence::types::Attachment;
use openspace_shared::persistence::{AttachmentStore, PersistenceError};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::sql::{id_to_string, map_sql_error, not_found, ts_to_datetime};
use crate::Database;

/// SQLite implementation of [`AttachmentStore`].
#[derive(Debug, Clone)]
pub struct SqliteAttachmentStore {
    db: Database,
}

impl SqliteAttachmentStore {
    /// Build a store over an opened database handle.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl AttachmentStore for SqliteAttachmentStore {
    async fn put(&self, metadata: &Attachment, bytes: &[u8]) -> Result<(), PersistenceError> {
        let metadata_json = serde_json::to_string(metadata)
            .map_err(|err| PersistenceError::IoError(err.to_string()))?;
        let id = id_to_string!(metadata.id);
        let turn_id = metadata.turn_id.map(|id| id_to_string!(id));
        let mime = metadata.mime_type.clone();
        let bytes = bytes.to_vec();
        let created_at = metadata.created_at.timestamp();
        self.db
            .connection()
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO attachments (id, turn_id, kind, mime, data_blob, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(id) DO UPDATE SET
                        turn_id = excluded.turn_id,
                        kind = excluded.kind,
                        mime = excluded.mime,
                        data_blob = excluded.data_blob,
                        created_at = excluded.created_at",
                    params![id, turn_id, metadata_json, mime, bytes, created_at],
                )?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn get_metadata(&self, id: AttachmentId) -> Result<Option<Attachment>, PersistenceError> {
        let id = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, kind, created_at FROM attachments WHERE id = ?1",
                        [id],
                        attachment_from_row,
                    )
                    .optional()?)
            })
            .await
            .map_err(map_sql_error)
    }

    async fn get_bytes(&self, id: AttachmentId) -> Result<Vec<u8>, PersistenceError> {
        let id = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                conn.query_row(
                    "SELECT data_blob FROM attachments WHERE id = ?1",
                    [id.clone()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| not_found(format!("attachment/{id}")))
            })
            .await
            .map_err(map_sql_error)
    }

    async fn delete(&self, id: AttachmentId) -> Result<(), PersistenceError> {
        let id = id_to_string!(id);
        self.db
            .connection()
            .call(move |conn| {
                conn.execute("DELETE FROM attachments WHERE id = ?1", [id])?;
                Ok(())
            })
            .await
            .map_err(map_sql_error)
    }

    async fn list_for_chat(&self, chat_id: ChatId) -> Result<Vec<Attachment>, PersistenceError> {
        let chat_id = id_to_string!(chat_id);
        self.db
            .connection()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT a.id, a.kind, a.created_at
                     FROM attachments a
                     JOIN turns t ON t.id = a.turn_id
                     WHERE t.chat_id = ?1
                     ORDER BY a.created_at ASC",
                )?;
                let rows = stmt
                    .query_map([chat_id], attachment_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(map_sql_error)
    }
}

fn attachment_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Attachment> {
    let id: String = row.get(0)?;
    let payload: String = row.get(1)?;
    let mut attachment: Attachment = serde_json::from_str(&payload).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(err))
    })?;
    attachment.id = AttachmentId::from_uuid(Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::nil()));
    let created_at: i64 = row.get(2)?;
    attachment.created_at = ts_to_datetime(created_at)?;
    Ok(attachment)
}
