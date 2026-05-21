//! AI-specific error enum — [`AiError`].
//!
//! Lives next to the provider trait rather than in the crate-level
//! [`crate::error`] module because only the AI slice surfaces it.
//! Every variant describes a *category* of failure the agent loop and
//! the UI need to distinguish — auth failures rerun the login flow,
//! rate-limit failures back off, network failures retry, and so on.
//!
//! # Variant choices
//!
//! - [`AiError::Network`] — transport-level failure (DNS, TCP, TLS,
//!   read timeout). The string carries the underlying diagnostic.
//! - [`AiError::Auth`] — credentials missing, invalid, or expired.
//! - [`AiError::RateLimited`] — provider quota tripped. The optional
//!   `retry_after` lets the agent loop schedule a precise resume; when
//!   `None` the caller falls back to its own back-off.
//! - [`AiError::InvalidRequest`] — the request was rejected because
//!   it was malformed (bad parameter, oversized prompt, …) — i.e. a
//!   bug in the caller, not a transient failure to retry.
//! - [`AiError::ModelNotFound`] — the [`crate::ai::domain::ModelRef`]
//!   pointed at a model the provider does not advertise.
//! - [`AiError::ProviderError`] — anything provider-specific that does
//!   not fit the categories above. Carries the `provider_id` so logs
//!   stay attributable.
//! - [`AiError::Cancelled`] — the agent loop dropped the stream (user
//!   pressed stop, the conversation switched away, …). Treated as a
//!   non-error in most surfaces but still surfaced as an error variant
//!   so callers can branch on it explicitly instead of poking at a
//!   parallel cancellation flag.
//! - [`AiError::Unsupported`] — the request asked for a capability the
//!   selected model does not advertise (e.g. a vision-only model paired
//!   with an image part, or a text-only model paired with a tool-call
//!   turn). Distinct from [`AiError::InvalidRequest`] because the
//!   *request* is structurally fine — the *model* is the constraint.
//!   The agent loop can react by reselecting a model rather than
//!   surfacing a caller bug.
//! - [`AiError::Other`] — escape hatch for adapters that genuinely
//!   cannot map onto the categories above. The string describes the
//!   failure for log readers.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;
use thiserror::Error;

use crate::ai::domain::ModelRef;

/// Failures the AI provider trait raises during a chat call, model
/// listing, or anywhere else its surface returns `Result<_, AiError>`.
///
/// Marked `#[non_exhaustive]` so future PRDs can land additional
/// variants — for instance a structured `ContentFilter { reason }` or
/// a provider-side `Overloaded` — without breaking pattern matches in
/// downstream crates. Callers are required to use a wildcard arm.
///
/// `Serialize + Deserialize` so the variant survives a hop through
/// telemetry pipelines and snapshot tests; the [`Duration`] inside
/// [`AiError::RateLimited`] uses `chrono::Duration`-compatible serde
/// shape via the standard library impl (`{ secs, nanos }`).
#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AiError {
    /// Transport-level failure (DNS, TCP, TLS, read timeout).
    #[error("network error: {0}")]
    Network(String),

    /// Credentials missing, invalid, or expired. The string explains
    /// which credential and how it failed without leaking the secret.
    #[error("auth error: {0}")]
    Auth(String),

    /// Provider quota tripped. `retry_after` is the provider's
    /// suggested back-off window; `None` when the provider did not
    /// advertise one.
    #[error("rate limited{}", retry_after.map(|d| format!(" (retry after {}s)", d.as_secs())).unwrap_or_default())]
    RateLimited {
        /// Provider-suggested back-off window. The agent loop reads
        /// this when present; otherwise it picks its own back-off.
        retry_after: Option<Duration>,
    },

    /// Request was malformed. Distinct from [`AiError::Network`]
    /// because the agent loop should not retry — it should surface a
    /// caller bug to the user.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The [`ModelRef`] pointed at a model the provider does not
    /// advertise. Carries the original handle so the UI can render
    /// "model X is not available from provider Y" without threading
    /// it separately.
    #[error("model not found: {}/{}", .0.provider_id, .0.model_id)]
    ModelNotFound(ModelRef),

    /// Anything provider-specific that does not match the categories
    /// above. `provider_id` keeps logs attributable; `message` carries
    /// the diagnostic verbatim.
    #[error("provider {provider_id} error: {message}")]
    ProviderError {
        /// Identifier of the provider that raised the failure.
        provider_id: String,
        /// Provider-supplied diagnostic message.
        message: String,
    },

    /// The stream was cancelled by the agent loop (user pressed stop,
    /// the conversation switched away, …).
    #[error("cancelled")]
    Cancelled,

    /// Selected model does not advertise the capability the request
    /// needs. The string describes which capability tripped (e.g.
    /// `"image input on a non-vision model"`,
    /// `"tool calling on a tool-less model"`). Distinct from
    /// [`AiError::InvalidRequest`] because the request itself is
    /// well-formed — the constraint is the model's capability set.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Catch-all for failures that genuinely do not fit any other
    /// variant. Adapters should reach for a more specific variant
    /// first; `Other` is intentionally last in the list.
    #[error("other error: {0}")]
    Other(String),
}

assert_impl_all!(AiError: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips every variant through JSON. Locks the wire form so
    /// telemetry pipelines and snapshot tests do not silently break
    /// after a future variant rename.
    #[test]
    fn serde_round_trip_covers_every_variant() {
        let cases = [
            AiError::Network("connection reset".to_string()),
            AiError::Auth("missing API key".to_string()),
            AiError::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            },
            AiError::RateLimited { retry_after: None },
            AiError::InvalidRequest("temperature out of range".to_string()),
            AiError::ModelNotFound(ModelRef::new("local", "no-such-model")),
            AiError::ProviderError {
                provider_id: "local".to_string(),
                message: "service unavailable".to_string(),
            },
            AiError::Cancelled,
            AiError::Unsupported("image input on a non-vision model".to_string()),
            AiError::Other("unexpected".to_string()),
        ];

        for original in cases {
            let json = serde_json::to_string(&original).expect("serialize");
            let decoded: AiError = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(original, decoded, "round-trip mismatch for {original:?}");
        }
    }

    /// `Display` is what logs and the UI render. Lock the prefix per
    /// variant so a casual rename does not silently change user-facing
    /// copy.
    #[test]
    fn display_prefixes_describe_the_category() {
        assert_eq!(AiError::Network("x".into()).to_string(), "network error: x");
        assert_eq!(AiError::Auth("x".into()).to_string(), "auth error: x");
        assert_eq!(
            AiError::RateLimited {
                retry_after: Some(Duration::from_secs(5))
            }
            .to_string(),
            "rate limited (retry after 5s)"
        );
        assert_eq!(
            AiError::RateLimited { retry_after: None }.to_string(),
            "rate limited"
        );
        assert_eq!(
            AiError::InvalidRequest("x".into()).to_string(),
            "invalid request: x"
        );
        assert_eq!(
            AiError::ModelNotFound(ModelRef::new("p", "m")).to_string(),
            "model not found: p/m"
        );
        assert_eq!(
            AiError::ProviderError {
                provider_id: "p".into(),
                message: "m".into()
            }
            .to_string(),
            "provider p error: m"
        );
        assert_eq!(AiError::Cancelled.to_string(), "cancelled");
        assert_eq!(
            AiError::Unsupported("tool calling on a tool-less model".into()).to_string(),
            "unsupported: tool calling on a tool-less model"
        );
        assert_eq!(AiError::Other("x".into()).to_string(), "other error: x");
    }
}
