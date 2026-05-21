//! Sandbox-aware HTTP client.
//!
//! Every outbound request from this crate flows through
//! [`SandboxedHttpClient`]: it owns a pre-configured `reqwest::Client`
//! and consults the active policy before opening a connection.
//! Adapters never construct a bare `reqwest::Client` — that is the
//! contract that keeps the network sandbox enforceable.
//!
//! # Configuration
//!
//! The underlying transport is pinned with `rustls-tls`, HTTP/2
//! enabled (ALPN negotiates it on demand), a 10-second connect
//! timeout, and **no** read timeout. The missing read timeout is
//! deliberate: streaming responses (server-sent events, JSON-newline)
//! need to sit idle between chunks without the client tearing the
//! connection down on its own. Cancellation is driven by dropping
//! the response future instead — that's cheap and predictable.
//!
//! # Gate flow
//!
//! Every entry point runs the same three-stage flow:
//!
//! 1. Parse the URL and extract `(host, port)`.
//! 2. Call
//!    [`openspace_shared::sandbox::SandboxPolicy::check_network`].
//! 3. Dispatch on the
//!    [`openspace_shared::sandbox::SandboxDecision`]:
//!    - `Allow` → fire the request.
//!    - `Block(reason)` → return [`HttpClientError::SandboxBlocked`].
//!    - `RequireApproval(reason)` → call the approval channel and
//!      gate on the answer.
//!
//! The approval call is wrapped in a [`tokio::time::timeout`] so a
//! never-answered prompt cannot pin a request indefinitely. The
//! ceiling is configurable via
//! [`SandboxedHttpClient::with_approval_timeout`].

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use openspace_shared::sandbox::{SandboxDecision, SandboxPolicy};
use openspace_shared::tool::{ApprovalChannel, ApprovalDecision};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Url;
use serde::Serialize;

use crate::error::HttpClientError;

/// Default ceiling on how long the client waits for an approval
/// answer before treating the silence as
/// [`HttpClientError::ApprovalTimeout`]. Sixty seconds is the
/// pragmatic compromise: long enough that a human can switch
/// windows, short enough that a forgotten prompt does not pin a
/// background request indefinitely.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Default connect timeout. Matches the issue contract.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum body bytes captured into [`HttpClientError::Status`].
/// Eight KiB is generous for upstream JSON error envelopes while
/// still small enough to inline in a tool result.
const STATUS_BODY_PREVIEW_BYTES: usize = 8 * 1024;

/// HTTP client that gates every request through the active policy.
/// See the module-level docs for the full behaviour contract.
///
/// The client is cheap to clone — internally everything is wrapped
/// in `Arc`s — and therefore safe to share across tasks. Adapters
/// typically hold a single instance behind
/// `Arc<SandboxedHttpClient>`.
#[derive(Clone)]
pub struct SandboxedHttpClient {
    inner: reqwest::Client,
    policy: Arc<SandboxPolicy>,
    approval: Arc<dyn ApprovalChannel + Send + Sync>,
    approval_timeout: Duration,
}

impl SandboxedHttpClient {
    /// Construct a client bound to the given policy and approval
    /// channel. The underlying `reqwest::Client` is built with the
    /// pinned configuration (HTTP/2 over rustls, 10-second connect
    /// timeout, no read timeout).
    ///
    /// # Errors
    ///
    /// Returns [`HttpClientError::Transport`] if `reqwest` cannot
    /// build the client (typically a TLS backend mismatch on an
    /// unsupported platform).
    pub fn new(
        policy: SandboxPolicy,
        approval: Arc<dyn ApprovalChannel + Send + Sync>,
    ) -> Result<Self, HttpClientError> {
        let inner = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(Self {
            inner,
            policy: Arc::new(policy),
            approval,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
        })
    }

    /// Override the approval-wait ceiling. Returns `self` so the
    /// builder pattern flows: `client.with_approval_timeout(...)`.
    #[must_use]
    pub fn with_approval_timeout(mut self, timeout: Duration) -> Self {
        self.approval_timeout = timeout;
        self
    }

    /// Borrow the active policy. Mostly useful in tests; production
    /// code holds the policy elsewhere too.
    #[must_use]
    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// Issue a GET against `url`. The returned [`reqwest::Response`]
    /// can be drained with `.text()` / `.json()` or streamed via
    /// `.bytes_stream()`; callers wanting a stream up-front use
    /// [`Self::post_streaming`] instead.
    ///
    /// # Errors
    ///
    /// Any [`HttpClientError`] variant — see the module docs.
    pub async fn get(&self, url: &str) -> Result<reqwest::Response, HttpClientError> {
        let parsed = parse_url(url)?;
        self.gate(&parsed).await?;
        let resp = self.inner.get(parsed).send().await?;
        ensure_success(resp).await
    }

    /// Issue a POST with a JSON body against `url`. The body is
    /// serialised through `serde_json` and the `Content-Type`
    /// header set to `application/json` automatically by reqwest.
    ///
    /// # Errors
    ///
    /// Any [`HttpClientError`] variant — see the module docs.
    pub async fn post_json<B: Serialize + ?Sized>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<reqwest::Response, HttpClientError> {
        let parsed = parse_url(url)?;
        self.gate(&parsed).await?;
        let resp = self.inner.post(parsed).json(body).send().await?;
        ensure_success(resp).await
    }

    /// Issue a POST with a JSON body and return the response body
    /// as a stream of byte chunks. Adapters layer SSE / JSON-NL
    /// framing on top of this.
    ///
    /// Dropping the returned stream closes the underlying
    /// connection promptly — that is the cancellation primitive
    /// every adapter relies on.
    ///
    /// # Errors
    ///
    /// Any [`HttpClientError`] variant — see the module docs.
    pub async fn post_streaming<B: Serialize + ?Sized>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<impl Stream<Item = Result<Bytes, HttpClientError>> + Send + 'static, HttpClientError>
    {
        self.post_streaming_with_headers(url, body, &[]).await
    }

    /// Issue a POST with a JSON body and an explicit set of extra
    /// request headers, then return the response body as a stream of
    /// byte chunks.
    ///
    /// Adapter-specific headers (auth keys, wire-format version pins,
    /// telemetry markers) flow through here. Header names that fail to
    /// parse, or values that contain forbidden bytes, surface as
    /// [`HttpClientError::InvalidUrl`] with a `header:` prefix on the
    /// diagnostic — same precondition shape the URL parser uses, so
    /// the agent loop's existing handling continues to apply.
    ///
    /// # Errors
    ///
    /// Any [`HttpClientError`] variant — see the module docs.
    pub async fn post_streaming_with_headers<B: Serialize + ?Sized>(
        &self,
        url: &str,
        body: &B,
        headers: &[(&str, &str)],
    ) -> Result<impl Stream<Item = Result<Bytes, HttpClientError>> + Send + 'static, HttpClientError>
    {
        let parsed = parse_url(url)?;
        self.gate(&parsed).await?;
        let header_map = build_header_map(headers)?;
        let mut req = self.inner.post(parsed).json(body);
        if !header_map.is_empty() {
            req = req.headers(header_map);
        }
        let resp = req.send().await?;
        let resp = ensure_success_preserving_headers(resp).await?;
        Ok(futures::StreamExt::map(resp.bytes_stream(), |chunk| {
            chunk.map_err(HttpClientError::from)
        }))
    }

    /// Run the sandbox gate for `url`. On `RequireApproval`, this
    /// awaits the approval channel under a timeout.
    async fn gate(&self, url: &Url) -> Result<(), HttpClientError> {
        let host = url.host_str().ok_or(HttpClientError::MissingHost)?;
        let port = url
            .port_or_known_default()
            .ok_or(HttpClientError::MissingHost)?;

        match self.policy.check_network(host, port) {
            SandboxDecision::Allow => Ok(()),
            SandboxDecision::Block(reason) => Err(HttpClientError::SandboxBlocked(reason)),
            SandboxDecision::RequireApproval(reason) => {
                let summary = reason.summary.clone();
                let answer =
                    tokio::time::timeout(self.approval_timeout, self.approval.request(reason))
                        .await
                        .map_err(|_| HttpClientError::ApprovalTimeout(self.approval_timeout))?;
                match answer {
                    ApprovalDecision::Approved => Ok(()),
                    ApprovalDecision::Denied => Err(HttpClientError::SandboxDenied(summary)),
                    ApprovalDecision::Timeout => {
                        Err(HttpClientError::ApprovalTimeout(self.approval_timeout))
                    }
                    // `ApprovalDecision` is `#[non_exhaustive]` in
                    // `openspace-shared`. Any future variant the
                    // Domain layer adds defaults to deny-by-safety
                    // here so a new approval state cannot silently
                    // approve a network call this client has not
                    // been taught about yet.
                    _ => Err(HttpClientError::SandboxDenied(summary)),
                }
            }
        }
    }
}

impl std::fmt::Debug for SandboxedHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn ApprovalChannel` carries no `Debug` bound, so we
        // surface the concrete pieces and elide the channel.
        f.debug_struct("SandboxedHttpClient")
            .field("policy", &*self.policy)
            .field("approval_timeout", &self.approval_timeout)
            .finish_non_exhaustive()
    }
}

// Compile-time guarantee that the public surface stays Send + Sync,
// per the issue acceptance criteria.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SandboxedHttpClient>();
};

/// Parse `url` into a [`Url`], normalising the error into
/// [`HttpClientError::InvalidUrl`].
fn parse_url(url: &str) -> Result<Url, HttpClientError> {
    Url::parse(url).map_err(|e| HttpClientError::InvalidUrl(e.to_string()))
}

/// Convert a non-success response into [`HttpClientError::Status`]
/// with a bounded body preview. Successful responses pass through
/// untouched so streaming consumers can keep the body intact.
async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response, HttpClientError> {
    ensure_success_preserving_headers(resp).await
}

/// Same gate as [`ensure_success`] but additionally captures the
/// parsed `Retry-After` header into the resulting
/// [`HttpClientError::Status`]. Streaming entry points use this so
/// adapters that surface 429 → `RateLimited` can read the back-off
/// hint directly from the typed error without scraping the body.
async fn ensure_success_preserving_headers(
    resp: reqwest::Response,
) -> Result<reqwest::Response, HttpClientError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let retry_after = parse_retry_after(resp.headers());
    let bytes = resp.bytes().await.unwrap_or_default();
    let take = bytes.len().min(STATUS_BODY_PREVIEW_BYTES);
    let body = String::from_utf8_lossy(&bytes[..take]).into_owned();
    Err(HttpClientError::Status {
        status,
        body,
        retry_after,
    })
}

/// Convert the input slice of `(name, value)` pairs into a
/// [`HeaderMap`]. Returns
/// [`HttpClientError::InvalidUrl`] (with a `header:` prefix) when a
/// pair fails to parse — adapters expose this as an
/// [`crate::error::HttpClientError`] precondition exactly the same way
/// a malformed URL surfaces, so the agent loop's existing handling
/// applies. The `InvalidUrl` reuse is deliberate: both failures share
/// the same caller-bug shape, and adding a dedicated `InvalidHeader`
/// variant would expand the public error surface for a single
/// precondition the caller controls end-to-end.
fn build_header_map(pairs: &[(&str, &str)]) -> Result<HeaderMap, HttpClientError> {
    let mut map = HeaderMap::with_capacity(pairs.len());
    for (name, value) in pairs {
        let name: HeaderName = name
            .parse()
            .map_err(|e: reqwest::header::InvalidHeaderName| {
                HttpClientError::InvalidUrl(format!("header name: {e}"))
            })?;
        let value = HeaderValue::from_str(value)
            .map_err(|e| HttpClientError::InvalidUrl(format!("header value: {e}")))?;
        map.append(name, value);
    }
    Ok(map)
}

/// Parse the `Retry-After` header into a [`Duration`].
///
/// Per RFC 9110, the header carries either a delta-seconds value
/// (a non-negative decimal integer) or an HTTP-date. We accept the
/// integer form, which is what every modern AI provider uses; the
/// HTTP-date form is left as `None` because none of the documented
/// upstreams emit it. Returning `None` rather than erroring keeps the
/// 429 path resilient: a malformed value still yields a typed
/// `Status { status: 429, .. }` and the AI layer falls back to its
/// own back-off.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}
