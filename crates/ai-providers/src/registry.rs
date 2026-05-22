//! [`ProviderRegistry`] — the lookup surface the agent loop uses to
//! resolve an [`AiProvider`] handle by id.
//!
//! # What lives here
//!
//! - [`ProviderId`] — newtype wrapping the stable string identifier
//!   every adapter advertises through [`AiProvider::id`]. Lowercase,
//!   whitespace-free, suitable as a config key. The newtype keeps the
//!   "id is not arbitrary text" invariant at the type surface so call
//!   sites cannot accidentally pass a display name where an id is
//!   expected.
//! - [`ProviderHandle`] — public summary the registry surfaces from
//!   [`ProviderRegistry::list`]. Carries id, display name, and the
//!   capability snapshot every adapter advertises.
//! - [`ProviderConfig`] — in-memory shape capturing each adapter's
//!   required fields. The TOML loader (PRD-03 territory) parses raw
//!   config files into this enum; the registry does not touch the
//!   filesystem.
//! - [`ProviderRegistry`] — `BTreeMap<ProviderId, Arc<dyn AiProvider>>`
//!   wrapped behind `register` / `lookup` / `remove` / `list`, plus a
//!   [`ProviderRegistry::notify_change`] stub the future settings
//!   layer (PRD-03) will call when configuration changes.
//!
//! # What does *not* live here
//!
//! - File parsing — that lives in PRD-03's settings crate.
//! - Adapter construction itself — every variant of [`ProviderConfig`]
//!   carries the parameters [`build_provider`] needs to wire up an
//!   adapter; the secret store and HTTP client are passed in by the
//!   caller because they are process-wide singletons, not per-provider
//!   configuration.
//! - Live-reload semantics — [`ProviderRegistry::notify_change`] is a
//!   no-op + structured `tracing::debug!` for now; the contract is
//!   what matters. The settings layer will subscribe to it once the
//!   loader lands.
//!
//! # Construction flow
//!
//! ```ignore
//! let mut registry = ProviderRegistry::new(
//!     Arc::clone(&secret_store),
//!     Arc::clone(&http),
//! );
//! registry.register(ProviderConfig::OpenAiCompatible { /* ... */ })?;
//! registry.register(ProviderConfig::Anthropic { /* ... */ })?;
//!
//! let provider = registry
//!     .lookup(&ProviderId::from_static("openai-compat-under-test"))
//!     .expect("registered above");
//! let stream = provider.chat_stream(conv, model_ref, params);
//! ```
//!
//! # Thread-safety
//!
//! The registry is `Send + Sync`. Mutation goes through `&mut self`
//! (the agent loop holds a single owner — typically behind a tokio
//! `RwLock` if multiple tasks need write access), reads go through
//! `&self`. Stored adapters are `Arc<dyn AiProvider>` so a `lookup`
//! returns a cheap clone that can outlive the borrow.

use std::collections::BTreeMap;
use std::sync::Arc;

use openspace_shared::ai::domain::Capabilities;
use openspace_shared::ai::error::AiError;
use openspace_shared::ai::provider::AiProvider;
use openspace_shared::ai::secret::{SecretRef, SecretStore};
use static_assertions::assert_impl_all;

use crate::anthropic::AnthropicProvider;
use crate::client::SandboxedHttpClient;
use crate::openai_compatible::OpenAiCompatibleProvider;

// ─────────────────────────────────────────────────────────────────────
// ProviderId
// ─────────────────────────────────────────────────────────────────────

/// Stable identifier for a provider entry.
///
/// Wraps a [`String`] so the type system can distinguish a provider
/// id from arbitrary text. The id matches [`AiProvider::id`] verbatim
/// — adapters that advertise `"openai-compat"` register under
/// `ProviderId::from_static("openai-compat")`.
///
/// `BTreeMap` ordering follows the wrapped string's `Ord`, so
/// [`ProviderRegistry::list`] returns entries sorted by id — a stable
/// shape settings UIs can rely on without a secondary sort.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderId(String);

impl ProviderId {
    /// Build an id from any string-like input. The value is stored
    /// verbatim — no trimming, no case folding. Adapters that
    /// advertise mixed-case ids are taken at their word; the registry
    /// only enforces uniqueness.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Cheaper variant for the common compile-time-known case.
    #[must_use]
    pub fn from_static(id: &'static str) -> Self {
        Self(id.to_string())
    }

    /// Borrow the underlying id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

assert_impl_all!(ProviderId: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ProviderHandle — public listing entry
// ─────────────────────────────────────────────────────────────────────

/// Lightweight projection of a registered provider, suitable for
/// settings UIs and pickers.
///
/// Carries the three pieces of information the agent loop actually
/// renders: the stable id (used as the picker's selection value), the
/// display name (used as the label), and the capability snapshot the
/// adapter advertised at registration time. Per-model capabilities
/// still live on [`openspace_shared::ai::domain::ModelInfo`]; the
/// snapshot here is the *upper bound* the provider supports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProviderHandle {
    /// Stable identifier — matches [`AiProvider::id`].
    pub id: ProviderId,
    /// Human-friendly label rendered in pickers and headers.
    pub display_name: String,
    /// Provider-level capability snapshot.
    pub capabilities: Capabilities,
}

impl ProviderHandle {
    /// Build a handle. Pairs with `#[non_exhaustive]` — call sites
    /// go through this constructor instead of struct literals so a
    /// future field (e.g. `health: ProviderHealth`) stays additive.
    #[must_use]
    pub fn new(
        id: ProviderId,
        display_name: impl Into<String>,
        capabilities: Capabilities,
    ) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            capabilities,
        }
    }
}

assert_impl_all!(ProviderHandle: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ProviderConfig — in-memory configuration surface
// ─────────────────────────────────────────────────────────────────────

/// In-memory shape capturing each adapter's required fields.
///
/// The TOML loader (PRD-03) parses raw configuration files into this
/// enum; the registry takes the parsed shape, never the raw file.
/// Each variant carries the minimum surface a single adapter needs to
/// be constructed; optional knobs (extra headers, request-id header,
/// model filter, version pin) ride on optional fields with sensible
/// defaults.
///
/// New adapter families add a variant here. The enum is
/// `#[non_exhaustive]` so a downstream match must use a wildcard arm,
/// keeping later additions non-breaking.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ProviderConfig {
    /// Configuration for the industry-standard chat-completions
    /// adapter ([`OpenAiCompatibleProvider`]).
    ///
    /// `aggregator_preset` is the optional shorthand from Slice 7 —
    /// when callers pin a known aggregator the loader can stamp the
    /// `extra_headers` and `request_id_header` from a preset table
    /// instead of repeating them in every config file. The registry
    /// stays oblivious to which preset produced the values; it only
    /// sees the resolved fields.
    OpenAiCompatible {
        /// Stable id this adapter advertises and the registry indexes
        /// under.
        id: ProviderId,
        /// Human-readable label rendered in pickers.
        display_name: String,
        /// Origin (scheme + host + optional `/v1` prefix). The
        /// `/chat/completions` segment is appended internally.
        base_url: String,
        /// Handle the [`SecretStore`] resolves at request time.
        api_key_ref: SecretRef,
        /// Bespoke headers attached to every request. Empty by
        /// default.
        extra_headers: Vec<(String, String)>,
        /// Header name carrying the upstream's correlation id, if
        /// any. `None` disables the lookup.
        request_id_header: Option<String>,
        /// Optional substring filter for the eventual `list_models`
        /// implementation.
        model_filter: Option<String>,
        /// Resolved aggregator preset name, kept for telemetry /
        /// debugging. `None` means the caller wired the headers
        /// directly. The loader collapses the preset into
        /// `extra_headers` / `request_id_header` before reaching the
        /// registry, so this field is informational only.
        aggregator_preset: Option<String>,
    },
    /// Configuration for the alternative wire-format adapter
    /// ([`AnthropicProvider`]).
    Anthropic {
        /// Stable id this adapter advertises.
        id: ProviderId,
        /// Human-readable label rendered in pickers.
        display_name: String,
        /// Origin including scheme and host.
        base_url: String,
        /// Handle the [`SecretStore`] resolves at request time.
        api_key_ref: SecretRef,
        /// Optional override for the wire-format version pin. `None`
        /// keeps the adapter's documented default.
        version: Option<String>,
    },
}

impl ProviderConfig {
    /// Borrow the configured provider id without matching every
    /// variant explicitly.
    #[must_use]
    pub fn id(&self) -> &ProviderId {
        match self {
            Self::OpenAiCompatible { id, .. } | Self::Anthropic { id, .. } => id,
        }
    }

    /// Borrow the configured display name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            Self::OpenAiCompatible { display_name, .. } | Self::Anthropic { display_name, .. } => {
                display_name.as_str()
            }
        }
    }
}

assert_impl_all!(ProviderConfig: Send, Sync);
// ─────────────────────────────────────────────────────────────────────
// ProviderRegistry
// ─────────────────────────────────────────────────────────────────────

/// Registry the agent loop consults to resolve an [`AiProvider`] by
/// id.
///
/// Holds a `BTreeMap` so iteration order is stable (sorted by id).
/// The map values are `Arc<dyn AiProvider>` so a [`Self::lookup`]
/// returns a cheap clone the caller can move into a task without
/// borrowing the registry. The `secret_store` and `http` handles are
/// shared with every adapter the registry constructs — a single
/// process-wide secret store and HTTP client back the entire fleet,
/// which keeps the sandbox policy decision live in one place.
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<ProviderId, Arc<dyn AiProvider>>,
    secret_store: Arc<dyn SecretStore>,
    http: Arc<SandboxedHttpClient>,
}

assert_impl_all!(ProviderRegistry: Send, Sync);

impl ProviderRegistry {
    /// Build an empty registry. The settings layer (PRD-03) calls
    /// [`Self::register`] once per configured provider after parsing
    /// the config file.
    #[must_use]
    pub fn new(secret_store: Arc<dyn SecretStore>, http: Arc<SandboxedHttpClient>) -> Self {
        Self {
            providers: BTreeMap::new(),
            secret_store,
            http,
        }
    }

    /// Register a provider from its in-memory configuration shape.
    ///
    /// The registry constructs the concrete adapter for the variant,
    /// stamps it under [`ProviderConfig::id`], and returns the
    /// resulting [`ProviderHandle`] for callers that want to confirm
    /// what landed.
    ///
    /// # Errors
    ///
    /// - [`AiError::InvalidRequest`] when an entry with the same id
    ///   is already registered. Re-registering is intentionally a
    ///   surface error rather than a silent overwrite — the settings
    ///   layer remove-then-register on edit, which keeps the audit
    ///   trail explicit.
    pub fn register(&mut self, config: ProviderConfig) -> Result<ProviderHandle, AiError> {
        let id = config.id().clone();
        if self.providers.contains_key(&id) {
            return Err(AiError::InvalidRequest(format!(
                "provider {id} is already registered; remove it before re-registering"
            )));
        }
        let provider = build_provider(config, &self.secret_store, &self.http);
        let handle = ProviderHandle::new(
            ProviderId::new(provider.id().to_string()),
            provider.display_name().to_string(),
            provider.capabilities(),
        );
        self.providers.insert(id, provider);
        tracing::debug!(
            target: "openspace_ai_providers::registry",
            id = %handle.id,
            display_name = %handle.display_name,
            "provider registered",
        );
        Ok(handle)
    }

    /// Look up a registered provider. Returns a cheap clone of the
    /// stored `Arc<dyn AiProvider>` so the caller can `chat_stream`
    /// against it without holding a borrow on the registry.
    #[must_use]
    pub fn lookup(&self, id: &ProviderId) -> Option<Arc<dyn AiProvider>> {
        self.providers.get(id).cloned()
    }

    /// Remove a registered provider. Returns the dropped handle when
    /// an entry existed, `None` otherwise. The settings layer pairs
    /// this with [`Self::register`] when an edit lands.
    pub fn remove(&mut self, id: &ProviderId) -> Option<Arc<dyn AiProvider>> {
        let removed = self.providers.remove(id);
        if removed.is_some() {
            tracing::debug!(
                target: "openspace_ai_providers::registry",
                id = %id,
                "provider removed",
            );
        }
        removed
    }

    /// Snapshot every registered provider as a [`ProviderHandle`].
    /// Order follows the underlying `BTreeMap` — sorted by id — so
    /// settings UIs render entries deterministically without an
    /// extra sort.
    #[must_use]
    pub fn list(&self) -> Vec<ProviderHandle> {
        self.providers
            .iter()
            .map(|(id, provider)| {
                ProviderHandle::new(
                    id.clone(),
                    provider.display_name().to_string(),
                    provider.capabilities(),
                )
            })
            .collect()
    }

    /// Hook the future settings layer (PRD-03) will call when the
    /// configuration on disk changes.
    ///
    /// Today this is a no-op + structured `tracing::debug!`. The
    /// contract — *the caller passes a snapshot of the new
    /// configuration set, the registry reconciles* — lives at this
    /// boundary so the loader code can be wired up without churn
    /// when the reconciliation logic lands. A real implementation
    /// will diff the snapshot against [`Self::list`], call
    /// [`Self::remove`] for entries that disappeared, and call
    /// [`Self::register`] for entries that were added or changed.
    pub fn notify_change(&mut self, configs: &[ProviderConfig]) {
        tracing::debug!(
            target: "openspace_ai_providers::registry",
            registered = self.providers.len(),
            incoming = configs.len(),
            "notify_change stub invoked — reconciliation lands with PRD-03",
        );
    }

    /// Borrow the secret store the registry shares with every
    /// adapter. Useful in tests that want to seed credentials after
    /// the registry has been built.
    #[must_use]
    pub fn secret_store(&self) -> &Arc<dyn SecretStore> {
        &self.secret_store
    }

    /// Borrow the sandbox HTTP client the registry shares with every
    /// adapter.
    #[must_use]
    pub fn http(&self) -> &Arc<SandboxedHttpClient> {
        &self.http
    }
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Arc<dyn AiProvider>` carries no `Debug` bound — surface
        // the registered ids and elide the trait objects.
        f.debug_struct("ProviderRegistry")
            .field(
                "providers",
                &self.providers.keys().collect::<Vec<&ProviderId>>(),
            )
            .finish_non_exhaustive()
    }
}

// ─────────────────────────────────────────────────────────────────────
// Adapter construction
// ─────────────────────────────────────────────────────────────────────

/// Translate a [`ProviderConfig`] into an `Arc<dyn AiProvider>`.
///
/// Pulled out of [`ProviderRegistry::register`] so the registry
/// surface stays small. New variants extend the match here; the
/// registry itself stays oblivious to which adapter sits behind a
/// given id.
fn build_provider(
    config: ProviderConfig,
    secret_store: &Arc<dyn SecretStore>,
    http: &Arc<SandboxedHttpClient>,
) -> Arc<dyn AiProvider> {
    match config {
        ProviderConfig::OpenAiCompatible {
            id,
            display_name,
            base_url,
            api_key_ref,
            extra_headers,
            request_id_header,
            model_filter,
            aggregator_preset: _,
        } => {
            let mut provider = OpenAiCompatibleProvider::new(
                id.as_str().to_string(),
                display_name,
                base_url,
                api_key_ref,
                Arc::clone(secret_store),
                Arc::clone(http),
            );
            for (name, value) in extra_headers {
                provider = provider.with_extra_header(name, value);
            }
            if let Some(header) = request_id_header {
                provider = provider.with_request_id_header(header);
            }
            if let Some(filter) = model_filter {
                provider = provider.with_model_filter(filter);
            }
            Arc::new(provider)
        }
        ProviderConfig::Anthropic {
            id: _,
            display_name: _,
            base_url,
            api_key_ref,
            version,
        } => {
            // The alternative-wire-format adapter advertises a fixed
            // id and display name internally — the registry uses the
            // configured id only as the lookup key. The display name
            // surfaces from `AiProvider::display_name()` when callers
            // list the registry, which keeps the adapter the source
            // of truth for its label.
            let mut provider = AnthropicProvider::new(
                base_url,
                api_key_ref,
                (**http).clone(),
                Arc::clone(secret_store),
            );
            if let Some(v) = version {
                provider = provider.with_version(v);
            }
            Arc::new(provider)
        }
    }
}
