//! Shared SQLite helpers.

use openspace_shared::persistence::PersistenceError;

macro_rules! id_to_string {
    ($id:expr) => {
        $id.as_uuid().to_string()
    };
}

pub(crate) use id_to_string;

pub(crate) fn ts_to_datetime(ts: i64) -> rusqlite::Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName(format!("invalid timestamp: {ts}")))
}

pub(crate) fn not_found(message: String) -> tokio_rusqlite::Error {
    tokio_rusqlite::Error::Rusqlite(rusqlite::Error::InvalidParameterName(format!(
        "not found: {message}"
    )))
}

pub(crate) fn conflict(message: String) -> tokio_rusqlite::Error {
    tokio_rusqlite::Error::Rusqlite(rusqlite::Error::InvalidParameterName(format!(
        "conflict: {message}"
    )))
}

pub(crate) fn map_sql_error(err: tokio_rusqlite::Error) -> PersistenceError {
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
