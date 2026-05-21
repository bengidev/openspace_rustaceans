//! Internal abstraction over the OS keychain — [`KeyringBackend`].
//!
//! The public [`super::keyring_secret_store::KeyringSecretStore`] talks
//! to the OS keychain through this small trait rather than reaching for
//! [`keyring::Entry`] directly. Two reasons:
//!
//! 1. **Testability.** [`keyring::Entry`] is a concrete struct backed by
//!    target-gated platform implementations; on a CI runner without a
//!    populated keychain there is no way to exercise the
//!    "platform-unavailable" classification path through the real
//!    crate. A trait we control lets a unit test inject a stub that
//!    deterministically returns the unavailable variant, so the
//!    public-surface translation into
//!    [`openspace_shared::ai::secret::SecretError::Unavailable`] is
//!    covered on every host.
//!
//! 2. **Error classification stays in one place.** The trait surface
//!    exposes a tiny [`KeyringBackendError`] enum (just `Unavailable`
//!    vs `Other`); the wrapping store then maps that onto
//!    [`openspace_shared::ai::secret::SecretError`]. The real backend
//!    ([`RealKeyringBackend`]) is the only file that knows about the
//!    upstream `keyring::Error` enum, so a future crate-version bump
//!    is a single-file change.
//!
//! # `set_password` / `get_password` / `delete_password` — naming
//!
//! The trait mirrors the upstream crate's verbs. The wrapping
//! `KeyringSecretStore` exposes the four-method `SecretStore` shape;
//! `list_keys` lives entirely above this trait, on top of an index
//! entry stored through these three primitives.
//!
//! # `Send + Sync`
//!
//! The trait requires both so the wrapping store can hold the backend
//! behind an `Arc<dyn KeyringBackend>` and stay `Send + Sync`. The real
//! backend is a unit struct — trivially thread-safe; unit-test stubs
//! use `Mutex`-protected state so the bound holds for them too.

use keyring::{Entry, Error as KeyringError};

// ─────────────────────────────────────────────────────────────────────
// KeyringBackendError — the error surface this trait exposes. Kept
// deliberately narrow: the only classification that callers act on is
// "is the platform backend reachable?" Everything else collapses into
// `Other(String)` and is surfaced as
// `SecretError::Backend(String)` upstream.
// ─────────────────────────────────────────────────────────────────────

/// Failure surface of [`KeyringBackend`].
///
/// The public store maps these onto
/// [`openspace_shared::ai::secret::SecretError`]. The split is
/// intentional: the trait surface here describes what *kind* of
/// failure happened in OS-keychain terms, the public surface
/// describes what the caller can do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyringBackendError {
    /// The platform secret backend is not reachable — the
    /// secret-service IPC call failed, the credential store is
    /// missing on the host, the default store could not be
    /// initialised, etc. Surfaced upstream as
    /// [`openspace_shared::ai::secret::SecretError::Unavailable`].
    Unavailable(String),

    /// Any other backend-level failure — invalid input, ambiguous
    /// match, encoding error. Surfaced upstream as
    /// [`openspace_shared::ai::secret::SecretError::Backend`].
    Other(String),
}

// ─────────────────────────────────────────────────────────────────────
// KeyringBackend — the trait everything goes through. `set_password` /
// `get_password` / `delete_password` are the three primitives the
// wrapping store needs; `list_keys` is layered on top of these via an
// index entry, so the trait does not need to express enumeration.
// ─────────────────────────────────────────────────────────────────────

/// Three-method abstraction over an OS keychain.
///
/// Implementations:
/// - [`RealKeyringBackend`] — talks to the real OS keychain via
///   [`keyring::Entry`]. Production wiring.
/// - A unit-test stub in `keyring_secret_store::tests` returns a
///   deterministic [`KeyringBackendError::Unavailable`] so the
///   public-surface translation can be exercised on hosts without a
///   populated keychain.
pub(crate) trait KeyringBackend: Send + Sync {
    /// Insert or replace the password stored under `(service, key)`.
    ///
    /// # Errors
    ///
    /// Returns [`KeyringBackendError`] when the underlying keychain
    /// cannot be written.
    fn set_password(
        &self,
        service: &str,
        key: &str,
        value: &str,
    ) -> Result<(), KeyringBackendError>;

    /// Look up the password stored under `(service, key)`.
    ///
    /// Returns `Ok(None)` when no entry exists — a missing key is
    /// not an error at this layer, matching the
    /// [`openspace_shared::ai::secret::SecretStore::get`] contract.
    ///
    /// # Errors
    ///
    /// Returns [`KeyringBackendError`] when the underlying keychain
    /// cannot be read.
    fn get_password(&self, service: &str, key: &str)
        -> Result<Option<String>, KeyringBackendError>;

    /// Remove the entry stored under `(service, key)`. Idempotent —
    /// deleting a missing entry is `Ok(())`, mirroring
    /// [`openspace_shared::ai::secret::SecretStore::delete`].
    ///
    /// # Errors
    ///
    /// Returns [`KeyringBackendError`] when the underlying keychain
    /// rejects the delete.
    fn delete_password(&self, service: &str, key: &str) -> Result<(), KeyringBackendError>;
}

// ─────────────────────────────────────────────────────────────────────
// RealKeyringBackend — the production implementation. Single unit
// struct because every call goes through `keyring::Entry::new`,
// which already carries the `(service, key)` pair; there is no
// per-instance state to keep.
// ─────────────────────────────────────────────────────────────────────

/// Production [`KeyringBackend`] backed by [`keyring::Entry`].
#[derive(Debug, Default)]
pub(crate) struct RealKeyringBackend;

impl RealKeyringBackend {
    /// Build a real backend.
    ///
    /// Construction itself never fails; per-platform availability is
    /// observed lazily on the first operation, and any platform
    /// failure surfaces as [`KeyringBackendError::Unavailable`] from
    /// [`KeyringBackend::set_password`] /
    /// [`KeyringBackend::get_password`] /
    /// [`KeyringBackend::delete_password`] rather than panicking
    /// here. The wrapping store therefore stays constructible on
    /// every host, and the agent loop can decide whether to keep
    /// using it after the first call returns.
    pub(crate) fn new() -> Self {
        Self
    }
}

// Translate `keyring::Error` into `KeyringBackendError`.
//
// `op` is the verb being attempted ("set" / "get" / "delete") so
// the diagnostic tells the user which call hit the wall — handy when
// only delete succeeds because the entry never existed but get fails
// because the daemon dropped the connection between calls.
fn classify(op: &str, err: KeyringError) -> KeyringBackendError {
    match err {
        // Platform-unreachable failures: the call could not even
        // make it to the credential store. Map these to
        // `Unavailable` so the upstream `SecretStore` surface tells
        // the agent loop "switch backends" rather than "retry".
        KeyringError::PlatformFailure(inner) => {
            KeyringBackendError::Unavailable(format!("{op}: platform failure: {inner}"))
        }
        KeyringError::NoStorageAccess(inner) => {
            KeyringBackendError::Unavailable(format!("{op}: no storage access: {inner}"))
        }
        // Everything else is a per-call failure that does not
        // suggest the whole backend is unreachable; keep them on
        // the `Other` branch.
        other => KeyringBackendError::Other(format!("{op}: {other}")),
    }
}

// Build a `keyring::Entry` for `(service, key)`, classifying any
// constructor-side failure into our trait error. This lets each
// method body stay focused on the operation itself.
fn entry_for(op: &str, service: &str, key: &str) -> Result<Entry, KeyringBackendError> {
    Entry::new(service, key).map_err(|err| classify(op, err))
}

impl KeyringBackend for RealKeyringBackend {
    fn set_password(
        &self,
        service: &str,
        key: &str,
        value: &str,
    ) -> Result<(), KeyringBackendError> {
        let entry = entry_for("set", service, key)?;
        entry
            .set_password(value)
            .map_err(|err| classify("set", err))
    }

    fn get_password(
        &self,
        service: &str,
        key: &str,
    ) -> Result<Option<String>, KeyringBackendError> {
        let entry = entry_for("get", service, key)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            // `NoEntry` means "key does not exist" — not an error at
            // this layer; the trait contract returns `Ok(None)`.
            Err(KeyringError::NoEntry) => Ok(None),
            Err(err) => Err(classify("get", err)),
        }
    }

    fn delete_password(&self, service: &str, key: &str) -> Result<(), KeyringBackendError> {
        let entry = entry_for("delete", service, key)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            // Same idempotency contract as `InMemorySecretStore`: a
            // missing key is not a delete failure.
            Err(KeyringError::NoEntry) => Ok(()),
            Err(err) => Err(classify("delete", err)),
        }
    }
}
