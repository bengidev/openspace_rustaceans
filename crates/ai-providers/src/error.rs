//! Error types for the sandboxed HTTP client.
//!
//! [`HttpClientError`] is the single error every public entry point
//! on [`crate::SandboxedHttpClient`] returns. Variants intentionally
//! lean on `String` for human-readable copy where the source error
//! does not carry useful structured data: the agent loop renders
//! these into tool results, where prose travels better than nested
//! enums.

use std::time::Duration;

use thiserror::Error;

/// Categorised failure mode for any call placed through
/// [`crate::SandboxedHttpClient`].
///
/// The variants mirror the gate stages: policy refusals come first
/// (`SandboxBlocked`, `SandboxDenied`, `ApprovalTimeout`), followed
/// by precondition failures the client catches before reqwest gets
/// involved (`InvalidUrl`, `MissingHost`), then the transport-level
/// failures (`Transport`, `Status`).
///
/// `#[non_exhaustive]` is deliberate — adding a new gate stage
/// (rate-limit, retry-budget) in a later slice should not break
/// downstream `match` arms that already handle the common cases.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HttpClientError {
    /// The active policy returned `Block` for the requested
    /// host/port. The string is the rendered reason copy — it
    /// surfaces directly in tool results.
    #[error("network blocked by sandbox: {0}")]
    SandboxBlocked(String),

    /// The sandbox required approval and the user (or scripted
    /// stand-in) said no. The string carries the prompt summary the
    /// caller showed.
    #[error("network denied by user: {0}")]
    SandboxDenied(String),

    /// The sandbox required approval and no answer arrived inside
    /// the configured wait. The duration is the wait the channel
    /// was given so logs can pinpoint the configured ceiling.
    #[error("approval timed out after {0:?}")]
    ApprovalTimeout(Duration),

    /// The URL passed to the entry point did not parse. Carries the
    /// underlying parse error message so logs stay debuggable.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// The URL parsed but did not carry a host component. The
    /// sandbox gate cannot run on a hostless URL — typically a
    /// caller mistake (e.g. passing a path-only string).
    #[error("URL has no host component")]
    MissingHost,

    /// Anything `reqwest` itself surfaces — DNS, TLS, HTTP framing,
    /// timeouts. The source carries the structured `reqwest::Error`
    /// for downstream callers that want to inspect it.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The server responded with a non-success status. The body
    /// preview is bounded (8 KiB) so the agent loop can attach it
    /// to a tool result without flooding the conversation.
    #[error("HTTP {status}: {body}")]
    Status {
        /// The status code the upstream returned.
        status: u16,
        /// Truncated response body (≤ 8 KiB) for diagnostics.
        body: String,
    },
}

impl HttpClientError {
    /// Whether the error came from the sandbox gate rather than the
    /// network. Useful for the agent loop to decide whether retrying
    /// is meaningful (transport errors might recover; sandbox
    /// refusals will not until the policy changes).
    #[must_use]
    pub fn is_sandbox(&self) -> bool {
        matches!(
            self,
            Self::SandboxBlocked(_) | Self::SandboxDenied(_) | Self::ApprovalTimeout(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_sandbox_covers_three_gate_variants() {
        assert!(HttpClientError::SandboxBlocked("x".into()).is_sandbox());
        assert!(HttpClientError::SandboxDenied("x".into()).is_sandbox());
        assert!(HttpClientError::ApprovalTimeout(Duration::from_secs(5)).is_sandbox());
    }

    #[test]
    fn is_sandbox_false_for_transport_and_precondition() {
        assert!(!HttpClientError::InvalidUrl("nope".into()).is_sandbox());
        assert!(!HttpClientError::MissingHost.is_sandbox());
        let err = HttpClientError::Status {
            status: 500,
            body: "boom".into(),
        };
        assert!(!err.is_sandbox());
    }
}
