//! Secret-handle vocabulary — [`SecretRef`], [`SecretStore`],
//! [`SecretError`].
//!
//! Provider configuration must persist *which* credential to use without
//! ever persisting the credential itself. This module supplies the two
//! halves of that contract:
//!
//! - [`SecretRef`] is a small, serialisable handle. It travels in
//!   provider settings, on the wire to the persistence layer, and back
//!   — never carrying the actual secret. The settings file therefore
//!   stays safe to commit, share, or back up.
//! - [`SecretStore`] is the runtime-side trait that turns a [`SecretRef`]
//!   into a live secret value. The Domain layer only sees the trait
//!   surface; the OS-keychain-backed implementation lives in a
//!   downstream infrastructure crate, and tests swap in an in-memory
//!   double through the same trait.
//!
//! # Naming policy
//!
//! Secret keys follow the namespaced shape
//! `openspace.provider.<id>.api_key`. The constructor
//! [`SecretRef::provider_api_key`] is the only call site that should
//! produce keys for the AI provider slice; consumers that build keys by
//! hand are expected to mirror the dotted shape so the settings file
//! stays scannable and the keychain entries stay groupable.
//!
//! Third-party product names never enter the key namespace. The
//! `<id>` segment is the provider's own identifier
//! ([`super::provider::AiProvider::id`]), which is already required to
//! be a stable, abstract, lowercase token.
//!
//! # Why a trait, not a concrete store
//!
//! Production code reaches for an OS-keychain implementation; tests
//! must not (CI does not have a populated keychain, and an unattended
//! test should not pop a system unlock prompt). The trait keeps the
//! Domain layer agnostic — every consumer that needs to fetch a
//! credential takes `&dyn SecretStore` and the wiring layer decides
//! which implementation flows in.
//!
//! # `Send + Sync` and `dyn`-compatibility
//!
//! Both the trait and the handle are pinned `Send + Sync` at compile
//! time via `static_assertions::assert_impl_all`, and the trait is
//! [object-safe][object-safety] so `Arc<dyn SecretStore>` can be
//! shared across the agent loop's tasks the same way
//! [`super::provider::AiProvider`] is.
//!
//! [object-safety]: https://doc.rust-lang.org/reference/items/traits.html#object-safety

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;
use thiserror::Error;

// ─────────────────────────────────────────────────────────────────────
// SecretRef — the persisted handle. Newtype around `String` so a
// `SecretRef` cannot be silently confused with a free-form key when it
// flows through a settings struct, and so the wire form is a bare
// quoted string (matches every other id newtype in this crate).
// ─────────────────────────────────────────────────────────────────────

/// Persisted handle to a secret managed by a [`SecretStore`].
///
/// `SecretRef` is *only* the lookup key. It never carries the secret
/// value. Settings files that contain `SecretRef`s remain safe to
/// commit and share — the value lives in the OS keychain (or a
/// downstream test double) and is fetched at use time through a
/// [`SecretStore`].
///
/// The wire form is a bare string (`#[serde(transparent)]`), matching
/// the convention every other handle-shaped type in this crate uses.
/// Both JSON and TOML round-trip the value unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    /// Wrap an arbitrary lookup key.
    ///
    /// Prefer the namespaced constructors ([`Self::provider_api_key`])
    /// when one exists — they enforce the dotted naming policy at the
    /// call site instead of leaving it as a convention. `new` is the
    /// escape hatch for slices that have to round-trip a key produced
    /// elsewhere (a stored settings value, a migration script, …).
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Build the canonical handle for a provider's API key.
    ///
    /// Produces `openspace.provider.<provider_id>.api_key`. The
    /// `provider_id` is taken verbatim — no normalisation — because
    /// [`super::provider::AiProvider::id`] already pins the value to a
    /// lowercase, whitespace-free, abstract token. Re-normalising here
    /// would mask a bug at the provider layer.
    #[must_use]
    pub fn provider_api_key(provider_id: impl AsRef<str>) -> Self {
        Self(format!(
            "openspace.provider.{}.api_key",
            provider_id.as_ref()
        ))
    }

    /// Borrow the lookup key.
    ///
    /// Returned by reference so the handle stays the source of truth —
    /// callers that need an owned `String` reach for `.to_string()`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

assert_impl_all!(SecretRef: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// SecretError — failure surface of the trait. Kept tiny on purpose:
// the test double cannot fail, and the eventual keychain
// implementation maps every backend failure into the single
// `Backend(String)` variant. Additional categories (e.g. `Locked`,
// `Cancelled`) can land later behind `#[non_exhaustive]` without
// breaking match arms downstream.
// ─────────────────────────────────────────────────────────────────────

/// Failures raised by a [`SecretStore`] implementation.
///
/// Marked `#[non_exhaustive]` so future PRDs can add categories the
/// keychain backend cares about — for instance a structured `Locked`
/// or `UserCancelled` once the OS-keychain implementation lands —
/// without breaking pattern matches in downstream crates. Callers are
/// required to use a wildcard arm.
#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SecretError {
    /// The underlying secret store could not satisfy the request.
    /// The string carries the backend-level diagnostic; it never
    /// contains the secret value itself.
    #[error("secret store backend error: {0}")]
    Backend(String),
}

assert_impl_all!(SecretError: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// SecretStore — the trait every backend implements. Sync-returning on
// purpose: the production backend (`KeyringSecretStore`, downstream)
// wraps blocking OS APIs that are typically fast but can prompt the
// user; an `async` surface would buy nothing and force callers into
// an async runtime they may not need. The agent loop wraps the trait
// in `tokio::task::spawn_blocking` when it wants a non-blocking call.
// ─────────────────────────────────────────────────────────────────────

/// Read/write contract every secret backend implements.
///
/// The trait surface is intentionally small — four operations,
/// idempotent where the underlying store allows. Implementations:
///
/// - **OS-keychain** (production, downstream): wraps the platform
///   keychain. Blocking but fast. Lives in the `openspace-ai-providers`
///   crate so this Domain crate stays IO-free.
/// - **In-memory** (test fixture, downstream): a `BTreeMap` behind a
///   `Mutex`. Lives behind the `test-support` feature on
///   `openspace-ai-providers` so release builds never carry the
///   double.
///
/// # Method semantics
///
/// - [`Self::set`] — upsert. Replacing an existing entry is allowed
///   and produces no diagnostic.
/// - [`Self::get`] — `Ok(Some(value))` when the key exists,
///   `Ok(None)` when it does not. Backend failures surface as
///   `Err(_)`. Splitting "not found" from "failed to look up" lets
///   the agent loop distinguish a missing credential (prompt the user
///   to enter one) from a broken keychain (surface the diagnostic).
/// - [`Self::delete`] — idempotent. Deleting a missing key returns
///   `Ok(())` because the post-condition the caller cares about
///   ("the key is not in the store") is already satisfied.
/// - [`Self::list_keys`] — every key currently in the store, in an
///   implementation-defined order. The keys are returned in full —
///   they are lookup handles, not secrets — so the caller can render
///   them in a settings UI or compare against a settings snapshot.
///
/// # Errors
///
/// Every method returns [`SecretError`] when the backend cannot
/// satisfy the request. The in-memory implementation cannot fail; the
/// keychain implementation maps platform errors into
/// `SecretError::Backend(String)`.
pub trait SecretStore: Send + Sync {
    /// Insert or replace the value at `key`.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the underlying store rejects the
    /// write (e.g. keychain locked, permission denied).
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;

    /// Look up the value at `key`.
    ///
    /// Returns `Ok(Some(_))` when the key exists, `Ok(None)` when it
    /// does not. Distinguishing "missing" from "broken" is what makes
    /// this `Result<Option<_>, _>` instead of a flat `Result<_, _>`
    /// with a `NotFound` variant.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the underlying store cannot be
    /// read (e.g. keychain locked, IPC failure).
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;

    /// Remove the value at `key`. Idempotent — a missing key is not
    /// an error.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the underlying store rejects the
    /// delete (e.g. keychain locked, permission denied).
    fn delete(&self, key: &str) -> Result<(), SecretError>;

    /// Enumerate every key currently in the store.
    ///
    /// Order is implementation-defined; callers that need a stable
    /// ordering sort the result themselves. The list contains lookup
    /// handles only, never secret values.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the underlying store cannot be
    /// enumerated (e.g. keychain locked, IPC failure).
    fn list_keys(&self) -> Result<Vec<String>, SecretError>;
}

// `dyn SecretStore: Send + Sync` so the agent loop can store the
// trait object behind an `Arc` and share it across tasks. If a
// future change to the trait inadvertently breaks `dyn`-compatibility
// the assertion fails to compile and the regression is caught at
// the trait surface, not at the call site.
assert_impl_all!(dyn SecretStore: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON wire form for a `SecretRef` is a bare quoted string,
    /// matching every other handle-shaped newtype in this crate.
    /// Lock the shape explicitly — callers persist these refs inside
    /// larger settings blobs and any change to the encoding breaks
    /// every existing snapshot.
    #[test]
    fn secret_ref_json_wire_form_is_a_bare_string() {
        let handle = SecretRef::provider_api_key("local");
        let json = serde_json::to_string(&handle).expect("serialize");
        assert_eq!(json, "\"openspace.provider.local.api_key\"");
    }

    /// JSON round-trip locks the wire shape so a future refactor
    /// cannot silently switch to a nested object form.
    #[test]
    fn secret_ref_round_trips_through_json() {
        let original = SecretRef::provider_api_key("local");
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: SecretRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
        assert_eq!(decoded.as_str(), "openspace.provider.local.api_key");
    }

    /// TOML round-trip locks the settings-file shape. Provider
    /// configuration travels through TOML in PRD-03; pinning the wire
    /// form here means a settings refactor cannot silently break a
    /// `SecretRef` field.
    #[test]
    fn secret_ref_round_trips_through_toml() {
        // `toml` cannot serialise a bare string at the top level —
        // serialised values must live under a key. Wrap in a tiny
        // settings-shaped struct to mirror how PRD-03 will use it.
        #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
        struct ProviderSettings {
            api_key_ref: SecretRef,
        }

        let original = ProviderSettings {
            api_key_ref: SecretRef::provider_api_key("local"),
        };
        let toml_text = toml::to_string(&original).expect("serialize toml");
        assert!(
            toml_text.contains("api_key_ref = \"openspace.provider.local.api_key\""),
            "unexpected toml encoding: {toml_text}"
        );
        let decoded: ProviderSettings = toml::from_str(&toml_text).expect("deserialize toml");
        assert_eq!(original, decoded);
    }

    /// `provider_api_key` is the canonical constructor for the AI
    /// slice. Lock the namespacing rule so a casual refactor cannot
    /// silently change the keychain path that running installs read
    /// from.
    #[test]
    fn provider_api_key_uses_the_documented_namespace() {
        assert_eq!(
            SecretRef::provider_api_key("local").as_str(),
            "openspace.provider.local.api_key"
        );
        assert_eq!(
            SecretRef::provider_api_key("hosted-router").as_str(),
            "openspace.provider.hosted-router.api_key"
        );
    }

    /// Round-trips `SecretError` through JSON. The variant set is
    /// tiny today but locking the shape now means future telemetry
    /// pipelines that snapshot errors stay stable.
    #[test]
    fn secret_error_round_trips_through_json() {
        let original = SecretError::Backend("keychain locked".to_string());
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: SecretError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
        assert_eq!(
            decoded.to_string(),
            "secret store backend error: keychain locked"
        );
    }
}
