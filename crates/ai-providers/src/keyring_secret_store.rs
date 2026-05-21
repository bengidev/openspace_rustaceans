//! OS-keychain-backed `SecretStore` — [`KeyringSecretStore`].
//!
//! `InMemorySecretStore` (gated behind the `test-support` feature) is
//! the test-only counterpart this type pairs with. Production code
//! reaches for `KeyringSecretStore`; tests reach for the in-memory
//! double through the feature flag. Both implement the same
//! `SecretStore` trait so the agent loop only ever holds an
//! `Arc<dyn SecretStore>` and the wiring layer decides which side
//! flows in.
//!
//! # Architecture
//!
//! Every operation goes through a small private `KeyringBackend`
//! trait (see `crate::keyring_backend`). In production the backend
//! is `RealKeyringBackend`, which delegates to [`keyring::Entry`].
//! In unit tests the backend is a stub the test owns, so the
//! "platform unavailable" classification path can be exercised on
//! every host — including a headless Linux runner without a
//! secret-service daemon, where the real backend would simply error
//! at runtime instead of giving the test something to assert on.
//!
//! # `list_keys` — index entry
//!
//! The OS keychain interfaces this slice supports do not expose a
//! uniform "enumerate every entry under this service" primitive
//! across macOS, Windows, and Linux desktop secret-service. To keep
//! parity with `SecretStore::list_keys` without reaching for
//! per-platform native code, the store maintains a private index
//! entry alongside the real entries:
//!
//! - The index is stored at the same `service` namespace under the
//!   reserved key `INDEX_KEY`.
//! - The index value is a JSON array of every key the store has been
//!   asked to write.
//! - `SecretStore::set` adds the key to the index (no-op on a key
//!   that is already present); `SecretStore::delete` removes it;
//!   `SecretStore::list_keys` decodes it.
//!
//! Trade-off: the index can drift if another tool mutates the
//! keychain externally — for example, a user manually deleting an
//! entry through the OS GUI. Drift is recoverable (the next
//! `SecretStore::set` / `SecretStore::delete` reconciles the
//! affected key) and the failure mode is benign (a stale entry in
//! `list_keys` whose `SecretStore::get` now returns `Ok(None)`).
//! The alternative — refusing to enumerate at all — would force every
//! caller to special-case `KeyringSecretStore` versus
//! `InMemorySecretStore`, which the
//! trait contract explicitly forbids.
//!
//! # Linux secret-service
//!
//! Linux desktops route OS-keychain calls through D-Bus to a running
//! secret-service daemon. Headless or minimal Linux systems often
//! lack a running daemon; calls fail with an IPC error. The real
//! backend classifies that failure as
//! `KeyringBackendError::Unavailable`, and
//! this store maps it onto
//! [`openspace_shared::ai::secret::SecretError::Unavailable`] with a
//! non-empty `reason`. Callers should surface the reason to the user
//! and prompt for a workaround:
//!
//! - install and unlock a secret-service-compatible daemon on the
//!   host, or
//! - fall back to the in-memory store for ephemeral sessions
//!   (test-support only), or
//! - rely on a future settings-file fallback (out of scope for the
//!   current slice).
//!
//! # References
//!
//! - Issue #46 — KeyringSecretStore (OS-gated).
//! - Parent #3 — PRD-02 AI Provider Layer (BYOK).

use std::sync::Arc;

use openspace_shared::ai::secret::{SecretError, SecretStore};
use static_assertions::assert_impl_all;

use crate::keyring_backend::{KeyringBackend, KeyringBackendError, RealKeyringBackend};

/// Default `service` segment for keychain entries when the caller
/// does not pick one. Matches the dotted prefix
/// [`openspace_shared::ai::secret::SecretRef::provider_api_key`]
/// already uses for keys, so the keychain UI groups every OpenSpace
/// entry under one logical heading.
pub const DEFAULT_SERVICE: &str = "openspace";

/// Reserved key the [`KeyringSecretStore`] uses to persist its
/// `list_keys` index. Picked to be unmistakable inside the keychain
/// UI and to never collide with a real provider key, which always
/// follows the `openspace.provider.<id>.api_key` shape produced by
/// [`openspace_shared::ai::secret::SecretRef::provider_api_key`].
pub const INDEX_KEY: &str = "openspace.__index";

// ─────────────────────────────────────────────────────────────────────
// KeyringSecretStore — the public production type. Stores secrets in
// the OS keychain via the `keyring` crate; maintains a JSON-encoded
// index entry to back `list_keys`. Backend access goes through the
// private [`KeyringBackend`] trait so the unit tests can exercise the
// error-classification path with an injected stub.
// ─────────────────────────────────────────────────────────────────────

/// OS-keychain-backed implementation of `SecretStore`.
///
/// Construct via [`Self::new`] for the default service segment, or
/// [`Self::with_service`] when the caller wants a non-default
/// keychain group (typically the per-run-unique value the OS-gated
/// integration test uses to keep parallel runs from racing on a
/// shared keychain entry).
///
/// The store is `Send + Sync` so the agent loop can hold it behind
/// an `Arc<dyn SecretStore>` and share it across tasks. The
/// `Send + Sync` bound is checked at compile time via
/// [`static_assertions::assert_impl_all`] further down this module
/// — if a future change breaks the bound, the assertion fails to
/// compile.
pub struct KeyringSecretStore {
    /// Keychain `service` segment. Acts as the namespace every entry
    /// produced by this store lives under. Cloned on each backend
    /// call rather than borrowed because the trait surface takes
    /// `&str` and the borrow checker cannot see across the boxed
    /// trait object.
    service: String,
    /// Backend wired in at construction time. Production wires
    /// `RealKeyringBackend`; tests wire a stub that returns
    /// deterministic [`KeyringBackendError`] values so the
    /// public-surface translation is testable without a populated
    /// keychain.
    backend: Arc<dyn KeyringBackend>,
}

// Hand-rolled `Debug` because `dyn KeyringBackend` does not require
// `Debug` (forcing it onto the trait would leak implementation
// detail into the trait surface). Logging / debug-formatting a store
// produces the service segment plus an opaque marker for the
// backend, which is enough for diagnostic purposes without
// exposing internal state.
impl std::fmt::Debug for KeyringSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringSecretStore")
            .field("service", &self.service)
            .field("backend", &"<dyn KeyringBackend>")
            .finish()
    }
}

impl KeyringSecretStore {
    /// Build a store rooted at the [`DEFAULT_SERVICE`] segment.
    ///
    /// Construction never fails — per-platform availability is
    /// observed lazily on the first operation. A host without a
    /// reachable platform backend (a Linux desktop with no
    /// secret-service daemon running) surfaces the failure as
    /// [`openspace_shared::ai::secret::SecretError::Unavailable`] from
    /// the operation rather than
    /// here, so the agent loop can decide whether to keep using the
    /// store after the first call returns.
    #[must_use]
    pub fn new() -> Self {
        Self::with_service(DEFAULT_SERVICE)
    }

    /// Build a store rooted at a caller-supplied `service` segment.
    ///
    /// Useful for tests that need a unique keychain namespace per run
    /// (so parallel CI runs do not race on a shared entry) and for
    /// future deployments that want to host multiple OpenSpace
    /// installs side by side under distinct keychain groups.
    #[must_use]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            backend: Arc::new(RealKeyringBackend::new()),
        }
    }

    /// Build a store with an injected backend.
    ///
    /// `pub(crate)` on purpose: the unit tests in this module reach
    /// for it to install a stub, but downstream crates only ever see
    /// [`Self::new`] / [`Self::with_service`]. Keeping the backend
    /// trait private keeps the public surface small.
    ///
    /// Gated `#[cfg(test)]` because no production call site needs an
    /// injected backend today; the gate lets `cargo build` reject a
    /// future leak through the public surface without a `dead_code`
    /// allow list.
    #[cfg(test)]
    pub(crate) fn with_backend(backend: Arc<dyn KeyringBackend>, service: String) -> Self {
        Self { service, backend }
    }

    // ── index helpers ────────────────────────────────────────────

    /// Read the JSON-encoded `list_keys` index from the keychain.
    /// Returns an empty `Vec` if the index entry has never been
    /// written, so the first `SecretStore::set` call does not have
    /// to special-case "no index yet".
    fn read_index(&self) -> Result<Vec<String>, SecretError> {
        match self.backend.get_password(&self.service, INDEX_KEY) {
            Ok(None) => Ok(Vec::new()),
            Ok(Some(raw)) => serde_json::from_str::<Vec<String>>(&raw).map_err(|err| {
                // A corrupt index is a backend-level fault — it
                // does not mean the platform is unreachable, so it
                // surfaces as `Backend(_)` rather than
                // `Unavailable(_)`. The diagnostic carries enough
                // detail for an operator to inspect the keychain
                // entry by hand.
                SecretError::Backend(format!("list_keys index decode failed: {err}"))
            }),
            Err(err) => Err(map_backend_error(err)),
        }
    }

    /// Persist a new `list_keys` index. Always overwrites the
    /// previous value.
    fn write_index(&self, index: &[String]) -> Result<(), SecretError> {
        // Encoding a `Vec<String>` of dotted ASCII keys cannot fail
        // through `serde_json` — `expect` documents the invariant
        // and keeps the error path linear.
        let encoded =
            serde_json::to_string(index).expect("Vec<String> always serialises through serde_json");
        self.backend
            .set_password(&self.service, INDEX_KEY, &encoded)
            .map_err(map_backend_error)
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

// Translate a `KeyringBackendError` into the public `SecretError`
// surface. The split is the whole point of the backend trait: the
// trait classifies failures in OS-keychain terms, this function
// turns the classification into the contract the agent loop sees.
fn map_backend_error(err: KeyringBackendError) -> SecretError {
    match err {
        KeyringBackendError::Unavailable(reason) => SecretError::Unavailable { reason },
        KeyringBackendError::Other(reason) => SecretError::Backend(reason),
    }
}

impl SecretStore for KeyringSecretStore {
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        // Refuse the reserved index key so a buggy caller cannot
        // overwrite the index by accident. `Backend(_)` because this
        // is a per-call programming error, not a platform-availability
        // problem.
        if key == INDEX_KEY {
            return Err(SecretError::Backend(format!(
                "key {INDEX_KEY:?} is reserved by KeyringSecretStore"
            )));
        }

        // Write the value first; only register the key in the index
        // after the store accepts it so a failed write never leaves
        // an index entry that points at a missing value.
        self.backend
            .set_password(&self.service, key, value)
            .map_err(map_backend_error)?;

        let mut index = self.read_index()?;
        if !index.iter().any(|existing| existing == key) {
            index.push(key.to_string());
            // Sort so `list_keys` is deterministic regardless of
            // insertion order. Matches the
            // `BTreeMap`-backed `InMemorySecretStore` so callers
            // observing both implementations see the same shape.
            index.sort();
            self.write_index(&index)?;
        }
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        if key == INDEX_KEY {
            return Err(SecretError::Backend(format!(
                "key {INDEX_KEY:?} is reserved by KeyringSecretStore"
            )));
        }
        self.backend
            .get_password(&self.service, key)
            .map_err(map_backend_error)
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        if key == INDEX_KEY {
            return Err(SecretError::Backend(format!(
                "key {INDEX_KEY:?} is reserved by KeyringSecretStore"
            )));
        }

        // Delete the value first so the index never advertises a
        // key whose value is gone.
        self.backend
            .delete_password(&self.service, key)
            .map_err(map_backend_error)?;

        let mut index = self.read_index()?;
        let original_len = index.len();
        index.retain(|existing| existing != key);
        if index.len() != original_len {
            self.write_index(&index)?;
        }
        Ok(())
    }

    fn list_keys(&self) -> Result<Vec<String>, SecretError> {
        self.read_index()
    }
}

assert_impl_all!(KeyringSecretStore: Send, Sync);

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use openspace_shared::ai::secret::{SecretRef, SecretStore};

    use super::*;

    // ── stub backends ───────────────────────────────────────────

    /// Backend that always reports the platform as unreachable.
    /// Drives the
    /// [`openspace_shared::ai::secret::SecretError::Unavailable`]
    /// mapping test on
    /// every host, including the Linux CI runner where the real
    /// backend would simply produce the same outcome at runtime.
    struct AlwaysUnavailableBackend {
        reason: &'static str,
    }

    impl KeyringBackend for AlwaysUnavailableBackend {
        fn set_password(
            &self,
            _service: &str,
            _key: &str,
            _value: &str,
        ) -> Result<(), KeyringBackendError> {
            Err(KeyringBackendError::Unavailable(self.reason.to_string()))
        }

        fn get_password(
            &self,
            _service: &str,
            _key: &str,
        ) -> Result<Option<String>, KeyringBackendError> {
            Err(KeyringBackendError::Unavailable(self.reason.to_string()))
        }

        fn delete_password(&self, _service: &str, _key: &str) -> Result<(), KeyringBackendError> {
            Err(KeyringBackendError::Unavailable(self.reason.to_string()))
        }
    }

    /// Process-local stub backend that mimics the real keychain so
    /// the index logic and the four-method round-trip can be
    /// exercised without touching the OS. Keys are stored under a
    /// `(service, key)` tuple to mirror the real backend's
    /// namespacing exactly.
    #[derive(Default)]
    struct InMemoryBackend {
        store: Mutex<BTreeMap<(String, String), String>>,
    }

    impl KeyringBackend for InMemoryBackend {
        fn set_password(
            &self,
            service: &str,
            key: &str,
            value: &str,
        ) -> Result<(), KeyringBackendError> {
            self.store
                .lock()
                .expect("InMemoryBackend mutex poisoned")
                .insert((service.to_string(), key.to_string()), value.to_string());
            Ok(())
        }

        fn get_password(
            &self,
            service: &str,
            key: &str,
        ) -> Result<Option<String>, KeyringBackendError> {
            Ok(self
                .store
                .lock()
                .expect("InMemoryBackend mutex poisoned")
                .get(&(service.to_string(), key.to_string()))
                .cloned())
        }

        fn delete_password(&self, service: &str, key: &str) -> Result<(), KeyringBackendError> {
            self.store
                .lock()
                .expect("InMemoryBackend mutex poisoned")
                .remove(&(service.to_string(), key.to_string()));
            Ok(())
        }
    }

    fn store_with_backend(backend: Arc<dyn KeyringBackend>) -> KeyringSecretStore {
        KeyringSecretStore::with_backend(backend, DEFAULT_SERVICE.to_string())
    }

    // ── unavailable-backend mapping (acceptance criterion) ──────

    /// Headline acceptance criterion: a backend that reports the
    /// platform as unreachable surfaces as
    /// [`openspace_shared::ai::secret::SecretError::Unavailable`]
    /// with a non-empty `reason` on
    /// every public-surface method. Drives the Linux-fallback path
    /// on hosts that have a working keychain too, so a regression
    /// in the mapping is caught the same way on macOS and Windows.
    #[test]
    fn maps_backend_unavailable_into_secret_error_unavailable() {
        let store = store_with_backend(Arc::new(AlwaysUnavailableBackend {
            reason: "test-injected: secret-service unreachable",
        }));
        let key = SecretRef::provider_api_key("local");

        for outcome in [store.set(key.as_str(), "value"), store.delete(key.as_str())] {
            match outcome {
                Err(SecretError::Unavailable { reason }) => {
                    assert!(
                        !reason.is_empty(),
                        "Unavailable reason must be non-empty for surfaceable diagnostic"
                    );
                }
                other => panic!("expected SecretError::Unavailable, got {other:?}"),
            }
        }

        match store.get(key.as_str()) {
            Err(SecretError::Unavailable { reason }) => {
                assert!(!reason.is_empty(), "Unavailable reason must be non-empty");
            }
            other => panic!("expected SecretError::Unavailable, got {other:?}"),
        }

        match store.list_keys() {
            Err(SecretError::Unavailable { reason }) => {
                assert!(!reason.is_empty(), "Unavailable reason must be non-empty");
            }
            other => panic!("expected SecretError::Unavailable, got {other:?}"),
        }
    }

    // ── index round-trip on the in-memory backend ───────────────

    /// `set` / `get` / `delete` / `list_keys` round-trip through the
    /// in-memory backend. Locks the index-entry shape so a future
    /// refactor cannot silently change how `list_keys` enumerates
    /// keys.
    #[test]
    fn round_trips_through_index_backed_list_keys() {
        let backend = Arc::new(InMemoryBackend::default());
        let store = store_with_backend(backend.clone());

        let local = SecretRef::provider_api_key("local");
        let hosted = SecretRef::provider_api_key("hosted-router");

        assert!(store.list_keys().expect("empty list").is_empty());

        store.set(local.as_str(), "value-a").expect("set local");
        store.set(hosted.as_str(), "value-b").expect("set hosted");

        assert_eq!(
            store.get(local.as_str()).expect("get local"),
            Some("value-a".to_string())
        );
        assert_eq!(
            store.get(hosted.as_str()).expect("get hosted"),
            Some("value-b".to_string())
        );

        // Sorted because `set` sorts on insert. Locks the contract
        // so callers can rely on it without sorting again.
        assert_eq!(
            store.list_keys().expect("list"),
            vec![
                "openspace.provider.hosted-router.api_key".to_string(),
                "openspace.provider.local.api_key".to_string(),
            ]
        );

        // Idempotent re-set does not duplicate the index entry.
        store
            .set(local.as_str(), "value-a-updated")
            .expect("re-set local");
        assert_eq!(store.list_keys().expect("list").len(), 2);
        assert_eq!(
            store.get(local.as_str()).expect("get updated"),
            Some("value-a-updated".to_string())
        );

        store.delete(local.as_str()).expect("delete local");
        assert!(store
            .get(local.as_str())
            .expect("get after delete")
            .is_none());
        assert_eq!(
            store.list_keys().expect("list after delete"),
            vec!["openspace.provider.hosted-router.api_key".to_string()]
        );

        // Deleting a missing key is `Ok(())` and does not touch the
        // index — matches the `InMemorySecretStore` contract so
        // callers can swap implementations freely.
        store
            .delete(local.as_str())
            .expect("idempotent delete on missing key");
        assert_eq!(store.list_keys().expect("list").len(), 1);
    }

    /// The reserved index key is not accessible through the
    /// public surface. A bug elsewhere in the codebase that tries
    /// to write through `INDEX_KEY` directly trips a `Backend(_)`
    /// error instead of corrupting the index.
    #[test]
    fn rejects_writes_to_the_reserved_index_key() {
        let store = store_with_backend(Arc::new(InMemoryBackend::default()));

        match store.set(INDEX_KEY, "anything") {
            Err(SecretError::Backend(msg)) => assert!(msg.contains(INDEX_KEY)),
            other => panic!("expected Backend reservation error, got {other:?}"),
        }
        match store.get(INDEX_KEY) {
            Err(SecretError::Backend(msg)) => assert!(msg.contains(INDEX_KEY)),
            other => panic!("expected Backend reservation error, got {other:?}"),
        }
        match store.delete(INDEX_KEY) {
            Err(SecretError::Backend(msg)) => assert!(msg.contains(INDEX_KEY)),
            other => panic!("expected Backend reservation error, got {other:?}"),
        }
    }

    /// Confirms `KeyringSecretStore` plugs into the `dyn SecretStore`
    /// surface every consumer expects, mirroring the matching test on
    /// the in-memory secret-store fixture.
    #[test]
    fn behaves_as_a_dyn_secret_store() {
        let store: Arc<dyn SecretStore> =
            Arc::new(store_with_backend(Arc::new(InMemoryBackend::default())));
        let key = SecretRef::provider_api_key("local");
        store.set(key.as_str(), "via-dyn").expect("set");
        assert_eq!(
            store.get(key.as_str()).expect("get"),
            Some("via-dyn".to_string())
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// OS-gated live round-trip — only compiled on platforms whose
// keychain backend is reachable on every host the project ships on.
// Linux is excluded because a headless CI runner has no
// secret-service daemon; the unavailable-mapping unit test above
// already covers that path through the stub backend.
// ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod live_round_trip {
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    use openspace_shared::ai::secret::{SecretRef, SecretStore};

    use super::KeyringSecretStore;

    /// Build a `(service, key)` pair unique to this run so parallel
    /// CI runs (and parallel local invocations) do not race on a
    /// shared keychain entry. Combines the process id with the
    /// monotonic time-since-epoch in nanoseconds — collision odds
    /// are vanishingly small without pulling a uuid dep.
    fn unique_service() -> String {
        let pid = process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after UNIX epoch")
            .as_nanos();
        format!("openspace-test-{pid}-{nanos}")
    }

    /// Drop-guard that wipes every key the test wrote, so a failing
    /// assertion does not leave stale entries in the host keychain.
    /// Iterates through `list_keys` because the index already knows
    /// the full set.
    struct Cleanup<'store> {
        store: &'store KeyringSecretStore,
    }

    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            if let Ok(keys) = self.store.list_keys() {
                for key in keys {
                    let _ = self.store.delete(&key);
                }
            }
        }
    }

    /// Acceptance criterion: round-trip on macOS and Windows under
    /// `cfg(target_os)`. Exercises every `SecretStore` method
    /// against the real OS keychain, with a unique service name and
    /// drop-based cleanup so repeated runs stay hermetic.
    ///
    /// Marked `#[ignore]` because the host keychain typically
    /// requires interactive approval the first time an unsigned
    /// binary asks for access (a system prompt outside the test
    /// runner). Default `cargo test` therefore skips it; an
    /// operator validating the platform path runs:
    ///
    /// ```text
    /// cargo test -p openspace-ai-providers --features test-support \
    ///   keyring_secret_store::live_round_trip -- --ignored --nocapture
    /// ```
    ///
    /// CI should run the same command on macOS and Windows runners
    /// once a properly-signed test harness is in place.
    #[test]
    #[ignore = "requires interactive OS-keychain access approval"]
    fn round_trip_against_os_keychain() {
        let store = KeyringSecretStore::with_service(unique_service());
        let _cleanup = Cleanup { store: &store };

        let local = SecretRef::provider_api_key("local");
        let hosted = SecretRef::provider_api_key("hosted-router");

        assert!(store.list_keys().expect("empty list").is_empty());

        store.set(local.as_str(), "value-a").expect("set local");
        store.set(hosted.as_str(), "value-b").expect("set hosted");

        assert_eq!(
            store.get(local.as_str()).expect("get local"),
            Some("value-a".to_string())
        );
        assert_eq!(
            store.get(hosted.as_str()).expect("get hosted"),
            Some("value-b".to_string())
        );

        let mut listed = store.list_keys().expect("list");
        listed.sort();
        assert_eq!(
            listed,
            vec![
                "openspace.provider.hosted-router.api_key".to_string(),
                "openspace.provider.local.api_key".to_string(),
            ]
        );

        // Upsert is silent — index size stays the same, value
        // changes.
        store
            .set(local.as_str(), "value-a-updated")
            .expect("re-set local");
        assert_eq!(store.list_keys().expect("list").len(), 2);
        assert_eq!(
            store.get(local.as_str()).expect("get updated"),
            Some("value-a-updated".to_string())
        );

        store.delete(local.as_str()).expect("delete local");
        assert!(store
            .get(local.as_str())
            .expect("get after delete")
            .is_none());
        assert_eq!(
            store.list_keys().expect("list after delete"),
            vec!["openspace.provider.hosted-router.api_key".to_string()]
        );
    }
}
