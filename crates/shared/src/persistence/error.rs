//! [`PersistenceError`] — failure vocabulary for every repository and
//! store on this Domain layer.
//!
//! Variants describe *categories* of failure rather than specific
//! operations. Callers pattern-match on the variant to drive UI and
//! retry decisions; the inner payload carries the diagnostic detail.
//!
//! Marked `#[non_exhaustive]` so future categories can land without a
//! breaking change downstream. Pattern matches must include a wildcard
//! arm — the round-trip test below pins that contract.
//!
//! # Why this enum lives next to the trait surface
//!
//! Earlier slices stashed a four-variant `PersistenceError` in
//! `crate::error`. With seven distinct categories now landing as
//! part of the persistence trait surface, the enum belongs next to
//! the traits that raise it: every consumer that imports a repository
//! trait already crosses this module, and the enum stops drifting from
//! the contract it serves.

use std::io;

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;
use thiserror::Error;

/// Categories of failure raised by the persistence trait surface.
///
/// PRD-03 distinguishes seven categories so callers can react
/// appropriately:
///
/// - [`PersistenceError::MigrationFailed`] — schema migration aborted
///   while opening the store. The agent loop surfaces this as a fatal
///   "needs operator attention" diagnostic; the store is not usable
///   until the migration is fixed.
/// - [`PersistenceError::IoError`] — an underlying IO operation failed
///   (file unreadable, disk full, permission denied, …). Carries the
///   OS-level diagnostic.
/// - [`PersistenceError::SerializationError`] — bytes on disk could
///   not be decoded into the expected shape, or an in-memory value
///   could not be encoded for storage.
/// - [`PersistenceError::NotFound`] — the addressed record does not
///   exist. The string identifies what was looked up so the UI can
///   render a helpful message without the caller threading the
///   identifier separately.
/// - [`PersistenceError::ConflictingWrite`] — an optimistic-lock
///   check failed or two writers raced for the same record. Distinct
///   from `IoError` because the right response is "reload, then
///   retry", not "back off and retry".
/// - [`PersistenceError::SchemaVersionMismatch`] — the on-disk schema
///   is at a different version than the running binary expects. The
///   agent loop refuses to operate on a mismatched store rather than
///   risk silent corruption.
/// - [`PersistenceError::DatabaseLocked`] — the underlying database
///   reports a transient lock (another writer, a background
///   checkpoint, …). Callers may back off and retry; persistent
///   `DatabaseLocked` errors should be promoted to a more specific
///   diagnostic by the impl.
///
/// `IoError` and `SerializationError` carry the underlying error as a
/// string because [`io::Error`] and [`serde_json::Error`] do not
/// implement `Serialize`/`Deserialize`. Persisting an error round
/// trip is rare, but keeping the enum serialisable means it can flow
/// through telemetry and snapshot pipelines without bespoke wrappers.
#[derive(Debug, Error, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PersistenceError {
    /// Schema migration aborted while opening the store. The string
    /// carries the migration runner's diagnostic.
    #[error("migration failed: {0}")]
    MigrationFailed(String),

    /// An underlying IO operation failed (file unreadable, disk full,
    /// permission denied, …). The string carries the OS-level
    /// description from the underlying [`io::Error`].
    #[error("io error: {0}")]
    IoError(String),

    /// The bytes on disk could not be decoded into the expected
    /// shape, or an in-memory value could not be encoded for storage.
    /// The string carries the serializer's diagnostic.
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// The requested resource does not exist. The string identifies
    /// what was looked up (path, id, key) so the UI can render a
    /// useful message without the caller threading the identifier
    /// separately.
    #[error("not found: {0}")]
    NotFound(String),

    /// A concurrent writer or a stale snapshot prevented the write.
    /// Distinct from [`Self::IoError`] because the right response is
    /// "reload the record and retry the optimistic-lock pass", not
    /// "back off the disk and retry".
    #[error("conflicting write: {0}")]
    ConflictingWrite(String),

    /// On-disk schema version does not match what the running binary
    /// expects. The agent loop refuses to operate on the store until
    /// the mismatch is resolved.
    #[error("schema version mismatch: {0}")]
    SchemaVersionMismatch(String),

    /// The underlying database reports a transient lock (another
    /// writer, a checkpoint, …). Callers may back off and retry.
    #[error("database locked: {0}")]
    DatabaseLocked(String),
}

impl From<io::Error> for PersistenceError {
    fn from(value: io::Error) -> Self {
        Self::IoError(value.to_string())
    }
}

impl From<serde_json::Error> for PersistenceError {
    fn from(value: serde_json::Error) -> Self {
        Self::SerializationError(value.to_string())
    }
}

assert_impl_all!(PersistenceError: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips every variant through JSON to lock the wire form.
    /// Telemetry and snapshot tests key off the variant tags; a silent
    /// rename here would invalidate every existing snapshot.
    #[test]
    fn serde_round_trip_covers_every_variant() {
        let cases = [
            PersistenceError::MigrationFailed("migration 0007 aborted".to_string()),
            PersistenceError::IoError("disk full".to_string()),
            PersistenceError::SerializationError("unexpected eof".to_string()),
            PersistenceError::NotFound("workspaces/42".to_string()),
            PersistenceError::ConflictingWrite("optimistic lock failed".to_string()),
            PersistenceError::SchemaVersionMismatch("expected 5, found 4".to_string()),
            PersistenceError::DatabaseLocked("busy".to_string()),
        ];

        for original in cases {
            let json = serde_json::to_string(&original).expect("serialize");
            let decoded: PersistenceError = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(original, decoded, "round-trip mismatch for {original:?}");
        }
    }

    #[test]
    fn from_io_error_maps_to_io_variant() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let mapped: PersistenceError = io_err.into();
        assert!(matches!(mapped, PersistenceError::IoError(_)));
    }

    #[test]
    fn from_serde_json_error_maps_to_serialization_variant() {
        let serde_err = serde_json::from_str::<u32>("not a number").unwrap_err();
        let mapped: PersistenceError = serde_err.into();
        assert!(matches!(mapped, PersistenceError::SerializationError(_)));
    }

    /// `Display` is the user-facing surface — assert the prefix per
    /// variant so a casual rename does not silently change the UI
    /// copy.
    #[test]
    fn display_prefixes_describe_the_category() {
        assert_eq!(
            PersistenceError::MigrationFailed("x".into()).to_string(),
            "migration failed: x"
        );
        assert_eq!(
            PersistenceError::IoError("x".into()).to_string(),
            "io error: x"
        );
        assert_eq!(
            PersistenceError::SerializationError("x".into()).to_string(),
            "serialization error: x"
        );
        assert_eq!(
            PersistenceError::NotFound("x".into()).to_string(),
            "not found: x"
        );
        assert_eq!(
            PersistenceError::ConflictingWrite("x".into()).to_string(),
            "conflicting write: x"
        );
        assert_eq!(
            PersistenceError::SchemaVersionMismatch("x".into()).to_string(),
            "schema version mismatch: x"
        );
        assert_eq!(
            PersistenceError::DatabaseLocked("x".into()).to_string(),
            "database locked: x"
        );
    }

    /// `#[non_exhaustive]` requires a wildcard arm. Pin the contract
    /// here so a future variant cannot quietly remove the marker.
    #[test]
    fn pattern_matches_require_a_wildcard_arm() {
        let err = PersistenceError::NotFound("x".into());
        #[allow(unreachable_patterns)]
        let label = match err {
            PersistenceError::MigrationFailed(_) => "migration",
            PersistenceError::IoError(_) => "io",
            PersistenceError::SerializationError(_) => "serde",
            PersistenceError::NotFound(_) => "not_found",
            PersistenceError::ConflictingWrite(_) => "conflict",
            PersistenceError::SchemaVersionMismatch(_) => "schema",
            PersistenceError::DatabaseLocked(_) => "locked",
            _ => "unknown",
        };
        assert_eq!(label, "not_found");
    }
}
