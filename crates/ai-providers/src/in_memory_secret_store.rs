//! Process-local [`SecretStore`] double — [`InMemorySecretStore`].
//!
//! Tests need a [`SecretStore`] they can construct without touching
//! the OS keychain (CI lacks a populated keychain, and unattended runs
//! must not pop a system unlock prompt). This module supplies that
//! double behind the crate's `test-support` feature so release builds
//! never carry it.
//!
//! Storage is a `BTreeMap<String, String>` behind a `Mutex`. The
//! `BTreeMap` keeps `list_keys` deterministic (sorted) so tests can
//! assert ordering without sorting at the call site. The `Mutex` keeps
//! the type `Send + Sync` so it can stand in for an
//! `Arc<dyn SecretStore>` everywhere the production keychain backend
//! eventually will.
//!
//! # When to reach for it
//!
//! - Unit tests that need a fully-functional `SecretStore` without IO.
//! - Integration tests that want to seed credentials before exercising
//!   a provider adapter, and inspect / mutate them mid-test.
//! - `proptest` scenarios that drive a sequence of operations and
//!   assert the postconditions.
//!
//! Production code never reaches for this type — the manifest gating
//! enforces that statically.

use std::collections::BTreeMap;
use std::sync::Mutex;

use openspace_shared::ai::secret::{SecretError, SecretStore};

/// Process-local in-memory implementation of [`SecretStore`].
///
/// Backed by a `BTreeMap` (deterministic key ordering for assertions)
/// behind a `Mutex` (`Send + Sync`, lets tests share the store across
/// tasks via `Arc<dyn SecretStore>`).
///
/// Construct via [`Self::new`] or [`Default::default`]; both produce
/// an empty store. The store cannot fail in any of its operations,
/// but the trait surface still returns [`Result`] so swapping in the
/// production keychain backend later is a manifest-level change with
/// no callsite churn.
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    inner: Mutex<BTreeMap<String, String>>,
}

impl InMemorySecretStore {
    /// Build a fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// Mutex poisoning is treated as a programming bug — it can only happen
// if a panic occurred while another caller held the lock, and we do
// nothing fallible while holding the guard. Surfacing the poison as a
// `SecretError::Backend` would let downstream code paper over the
// underlying panic; panicking here keeps the bug visible.
impl SecretStore for InMemorySecretStore {
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.inner
            .lock()
            .expect("InMemorySecretStore mutex poisoned")
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self
            .inner
            .lock()
            .expect("InMemorySecretStore mutex poisoned")
            .get(key)
            .cloned())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.inner
            .lock()
            .expect("InMemorySecretStore mutex poisoned")
            .remove(key);
        Ok(())
    }

    fn list_keys(&self) -> Result<Vec<String>, SecretError> {
        Ok(self
            .inner
            .lock()
            .expect("InMemorySecretStore mutex poisoned")
            .keys()
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use openspace_shared::ai::secret::{SecretRef, SecretStore};

    use super::*;

    /// Headline acceptance criterion: the round-trip across all four
    /// operations works on a fresh store. Uses [`SecretRef`] for the
    /// keys so the test exercises both halves of the secret-handle
    /// vocabulary at once.
    #[test]
    fn set_get_delete_list_keys_round_trip() {
        let store = InMemorySecretStore::new();
        let local_key = SecretRef::provider_api_key("local");
        let hosted_key = SecretRef::provider_api_key("hosted-router");

        // Empty on construction.
        assert!(store.list_keys().expect("list").is_empty());
        assert!(store.get(local_key.as_str()).expect("get").is_none());

        // `set` upserts. Two distinct keys land independently.
        store.set(local_key.as_str(), "value-a").expect("set");
        store.set(hosted_key.as_str(), "value-b").expect("set");

        // `get` returns the stored value.
        assert_eq!(
            store.get(local_key.as_str()).expect("get"),
            Some("value-a".to_string())
        );
        assert_eq!(
            store.get(hosted_key.as_str()).expect("get"),
            Some("value-b".to_string())
        );

        // `list_keys` enumerates every key. Sorted because the
        // backing map is a `BTreeMap`.
        assert_eq!(
            store.list_keys().expect("list"),
            vec![
                "openspace.provider.hosted-router.api_key".to_string(),
                "openspace.provider.local.api_key".to_string(),
            ]
        );

        // `set` again replaces in place rather than appending.
        store
            .set(local_key.as_str(), "value-a-updated")
            .expect("set");
        assert_eq!(
            store.get(local_key.as_str()).expect("get"),
            Some("value-a-updated".to_string())
        );
        assert_eq!(store.list_keys().expect("list").len(), 2);

        // `delete` removes a single key.
        store.delete(local_key.as_str()).expect("delete");
        assert!(store.get(local_key.as_str()).expect("get").is_none());
        assert_eq!(
            store.list_keys().expect("list"),
            vec!["openspace.provider.hosted-router.api_key".to_string()]
        );
    }

    /// `delete` is idempotent — calling it twice for the same key
    /// (or once for a never-set key) is `Ok(())`. Pinning this here
    /// makes the contract explicit so a future backend cannot
    /// silently start erroring on missing keys.
    #[test]
    fn delete_is_idempotent_for_missing_keys() {
        let store = InMemorySecretStore::new();
        let key = SecretRef::provider_api_key("local");

        store.delete(key.as_str()).expect("delete on empty store");
        store.set(key.as_str(), "v").expect("set");
        store.delete(key.as_str()).expect("first delete");
        store.delete(key.as_str()).expect("second delete");
        assert!(store.get(key.as_str()).expect("get").is_none());
    }

    /// Confirms `InMemorySecretStore` plugs into the `dyn SecretStore`
    /// surface the agent loop expects. The compile-time
    /// `assert_impl_all!(dyn SecretStore: Send, Sync)` in the trait
    /// module covers `Send + Sync`; this test covers the runtime
    /// behaviour through the trait object.
    #[test]
    fn behaves_as_a_dyn_secret_store() {
        let store: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
        let key = SecretRef::provider_api_key("local");
        store.set(key.as_str(), "via-dyn").expect("set");
        assert_eq!(
            store.get(key.as_str()).expect("get"),
            Some("via-dyn".to_string())
        );
    }
}
