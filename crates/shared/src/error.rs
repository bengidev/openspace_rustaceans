//! Crate-level error scaffolding.
//!
//! Houses error enums shared by more than one concern. The rule of
//! thumb is the same as for any other type in this crate: if a single
//! module owns the error, it lives in that module (e.g. `AiError` in
//! `ai::error`, `ToolError` in `tool`); only errors crossed by
//! multiple modules belong here.
//!
//! [`PersistenceError`] qualifies because the `ai`, `tool`, and
//! `command` slices all need to surface "could not load / save state"
//! failures and we want a single canonical type for that.

use std::io;

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;
use thiserror::Error;

/// Failures the persistence layer (PRD-03) raises when reading or
/// writing snapshots, configs, and other on-disk state.
///
/// The variants are deliberately broad — they describe the *category*
/// of failure, not the specific operation. Callers pattern-match on the
/// variant to drive UI ("file not found" vs "another writer holds the
/// lock") and let the inner payload carry the diagnostic detail.
///
/// `Io` and `Serialization` carry the underlying error as a string
/// because [`io::Error`] and [`serde_json::Error`] are not
/// `Serialize` / `Deserialize` themselves. Persisting an error round
/// trip is rare, but keeping the enum serialisable means it can flow
/// through telemetry and snapshot pipelines without bespoke wrappers.
///
/// Marked `#[non_exhaustive]` so adding new variants in later PRDs is
/// non-breaking; downstream callers must include a wildcard arm.
#[derive(Debug, Error, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PersistenceError {
    /// An underlying IO operation failed (file not readable, disk
    /// full, permission denied, …). The string carries the OS-level
    /// description from the underlying [`io::Error`].
    #[error("io error: {0}")]
    Io(String),

    /// The bytes on disk could not be decoded into the expected shape,
    /// or an in-memory value could not be encoded for storage. The
    /// string carries the serializer's diagnostic.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// The requested resource does not exist. The string identifies
    /// what was looked up (path, id, key) so the UI can render a
    /// useful message without the caller threading the identifier
    /// separately.
    #[error("not found: {0}")]
    NotFound(String),

    /// A concurrent writer or a stale snapshot prevented the write.
    /// The persistence layer raises this when an optimistic lock check
    /// fails or when two writers race for the same resource. The
    /// string describes the conflict in user-facing terms.
    #[error("conflict: {0}")]
    Conflict(String),
}

impl From<io::Error> for PersistenceError {
    fn from(value: io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<serde_json::Error> for PersistenceError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}

assert_impl_all!(PersistenceError: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips every variant through JSON to lock the wire format.
    /// Acceptance criteria for issue #27 require persisted types to
    /// round-trip; `PersistenceError` is `Serialize + Deserialize` so
    /// it inherits that contract even though it is rarely persisted.
    #[test]
    fn serde_round_trip_covers_every_variant() {
        let cases = [
            PersistenceError::Io("disk full".to_string()),
            PersistenceError::Serialization("unexpected eof".to_string()),
            PersistenceError::NotFound("workspace.json".to_string()),
            PersistenceError::Conflict("snapshot version mismatch".to_string()),
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
        assert!(matches!(mapped, PersistenceError::Io(_)));
    }

    #[test]
    fn from_serde_json_error_maps_to_serialization_variant() {
        let serde_err = serde_json::from_str::<u32>("not a number").unwrap_err();
        let mapped: PersistenceError = serde_err.into();
        assert!(matches!(mapped, PersistenceError::Serialization(_)));
    }

    /// Display output is the user-facing surface — assert the prefix
    /// for each variant so a casual rename does not silently change
    /// the UI copy.
    #[test]
    fn display_prefixes_describe_the_category() {
        assert_eq!(PersistenceError::Io("x".into()).to_string(), "io error: x");
        assert_eq!(
            PersistenceError::Serialization("x".into()).to_string(),
            "serialization error: x"
        );
        assert_eq!(
            PersistenceError::NotFound("x".into()).to_string(),
            "not found: x"
        );
        assert_eq!(
            PersistenceError::Conflict("x".into()).to_string(),
            "conflict: x"
        );
    }
}
