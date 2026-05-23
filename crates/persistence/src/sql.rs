//! Shared SQLite helpers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use openspace_shared::persistence::PersistenceError;
use rusqlite::TransactionBehavior;

use crate::Database;

const WRITE_RETRY_BUDGET: Duration = Duration::from_secs(5);
const WRITE_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const WRITE_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(250);

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

pub(crate) async fn write_transaction<T, F>(db: &Database, op: F) -> Result<T, PersistenceError>
where
    T: Send + 'static,
    F: Fn(&rusqlite::Transaction<'_>) -> tokio_rusqlite::Result<T> + Send + Sync + 'static,
{
    let started = Instant::now();
    let mut backoff = WRITE_RETRY_INITIAL_BACKOFF;
    let op = Arc::new(op);

    loop {
        let op_for_call = Arc::clone(&op);
        let result = db
            .connection()
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(tokio_rusqlite::Error::from)?;
                let output = op_for_call(&tx)?;
                tx.commit().map_err(tokio_rusqlite::Error::from)?;
                Ok(output)
            })
            .await;

        match result {
            Ok(output) => return Ok(output),
            Err(err) if is_busy(&err) && started.elapsed() < WRITE_RETRY_BUDGET => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(WRITE_RETRY_MAX_BACKOFF);
            }
            Err(err) => return Err(map_sql_error(err)),
        }
    }
}

fn is_busy(err: &tokio_rusqlite::Error) -> bool {
    matches!(
        err,
        tokio_rusqlite::Error::Rusqlite(rusqlite::Error::SqliteFailure(code, _))
            if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
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
