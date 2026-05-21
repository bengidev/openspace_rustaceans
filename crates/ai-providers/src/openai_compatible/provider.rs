//! [`OpenAiCompatibleProvider`] — adapter for the industry-standard
//! `/v1/chat/completions` wire format.
//!
//! This slice covers text-only streaming and full error
//! classification. Tool calling, vision parts, and `list_models`
//! land in the follow-up slice — `chat_stream` rejects non-text
//! parts with [`AiError::InvalidRequest`] and `list_models` returns
//! an empty list.
//!
//! # Construction
//!
//! Adapters configure five fields:
//!
//! - `base_url` — origin including scheme and (optionally) port. The
//!   `/v1/chat/completions` path is appended internally; callers that
//!   want a different path layer that on top by passing the full
//!   prefix.
//! - `api_key_ref` — handle the [`SecretStore`] resolves at request
//!   time. The provider never holds the secret value across calls.
//! - `extra_headers` — bespoke headers attached to every request
//!   (e.g. a vendor-specific organisation id).
//! - `request_id_header` — header name to receive the
//!   correlation id when the upstream advertises one. Stored
//!   verbatim; this slice does not yet thread the value into the
//!   stream output.
//! - `model_filter` — optional substring filter for the
//!   future `list_models` implementation. Stored as configured but
//!   unused in this slice.
//!
//! # Streaming protocol
//!
//! Every chunk on the wire arrives as an SSE `data:` event whose
//! payload is a single JSON object. The parser yields
//! [`StreamEvent::TextDelta`] for every non-empty content delta,
//! [`StreamEvent::UsageReport`] when the final chunk carries a
//! usage block, and [`StreamEvent::Done`] when either the upstream
//! emits `[DONE]` or the stream closes cleanly.
//!
//! Cancellation is driven by dropping the returned `BoxStream` —
//! the underlying `reqwest::Response` body is dropped with it,
//! which closes the TCP connection. This is the same primitive the
//! sandbox client's integration test pins.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use openspace_shared::ai::domain::{
    Capabilities, Conversation, GenerationParams, ModelInfo, ModelRef, Part, Role, Usage,
};
use openspace_shared::ai::error::AiError;
use openspace_shared::ai::provider::{AiProvider, StreamEvent};
use openspace_shared::ai::secret::{SecretRef, SecretStore};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use static_assertions::assert_impl_all;

use crate::client::SandboxedHttpClient;

use super::error_map::map_status;
use super::sse::SseParser;
use super::wire::{ChatMessage, ChatRequest, ChatStreamChunk, StreamOptions};

/// Provider adapter for the industry-standard chat-completions wire
/// format. See the module docs for the configuration surface and
/// streaming contract.
#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    id: String,
    display_name: String,
    base_url: String,
    api_key_ref: SecretRef,
    secret_store: Arc<dyn SecretStore>,
    http: Arc<SandboxedHttpClient>,
    extra_headers: Vec<(String, String)>,
    request_id_header: Option<String>,
    model_filter: Option<String>,
}

assert_impl_all!(OpenAiCompatibleProvider: Send, Sync);

impl OpenAiCompatibleProvider {
    /// Construct a provider with the minimum required configuration.
    /// Optional knobs (`extra_headers`, `request_id_header`,
    /// `model_filter`) flow in via the builder methods below.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        base_url: impl Into<String>,
        api_key_ref: SecretRef,
        secret_store: Arc<dyn SecretStore>,
        http: Arc<SandboxedHttpClient>,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            base_url: base_url.into(),
            api_key_ref,
            secret_store,
            http,
            extra_headers: Vec::new(),
            request_id_header: None,
            model_filter: None,
        }
    }

    /// Attach an extra header to every request. Repeated calls
    /// accumulate; same-name calls do not deduplicate — that is the
    /// caller's responsibility.
    #[must_use]
    pub fn with_extra_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Configure the response header that carries the correlation
    /// id. Stored verbatim; not threaded into the stream output in
    /// this slice (lands with the observability surface in PRD-05).
    #[must_use]
    pub fn with_request_id_header(mut self, name: impl Into<String>) -> Self {
        self.request_id_header = Some(name.into());
        self
    }

    /// Configure the substring filter the future `list_models`
    /// implementation will apply. Stored but unused this slice.
    #[must_use]
    pub fn with_model_filter(mut self, filter: impl Into<String>) -> Self {
        self.model_filter = Some(filter.into());
        self
    }

    /// Borrow the configured request-id header name. Useful for
    /// tests asserting the configuration round-trips.
    #[must_use]
    pub fn request_id_header(&self) -> Option<&str> {
        self.request_id_header.as_deref()
    }

    /// Borrow the configured model filter. Useful for tests
    /// asserting the configuration round-trips.
    #[must_use]
    pub fn model_filter(&self) -> Option<&str> {
        self.model_filter.as_deref()
    }
}

#[async_trait]
impl AiProvider for OpenAiCompatibleProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn capabilities(&self) -> Capabilities {
        // This slice ships text streaming only. Tool calling and
        // vision flip on in the follow-up slice.
        Capabilities::new(false, false, true)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        // `list_models` lands in the follow-up slice. Returning an
        // empty list keeps the trait surface honest without
        // pretending to enumerate.
        Ok(Vec::new())
    }

    fn chat_stream(
        &self,
        conv: Conversation,
        model: ModelRef,
        params: GenerationParams,
    ) -> BoxStream<'static, Result<StreamEvent, AiError>> {
        let provider_id = self.id.clone();
        let base_url = self.base_url.clone();
        let api_key_ref = self.api_key_ref.clone();
        let secret_store = Arc::clone(&self.secret_store);
        let http = Arc::clone(&self.http);
        let extra_headers = self.extra_headers.clone();

        // The future-of-stream pattern: we need to perform the HTTP
        // request before we have anything to stream, but the trait
        // surface is sync-returning. `stream::once(...).flatten()`
        // unwraps an inner stream produced by an async block.
        let work = async move {
            match open_stream(
                provider_id.clone(),
                base_url,
                api_key_ref,
                secret_store,
                http,
                extra_headers,
                conv,
                model,
                params,
            )
            .await
            {
                Ok(s) => s,
                Err(err) => stream::once(async move { Err(err) }).boxed(),
            }
        };

        stream::once(work).flatten().boxed()
    }
}

/// Build the request, dispatch it through the sandbox HTTP client,
/// and turn the byte stream into a [`StreamEvent`] stream.
///
/// Pulled out of [`AiProvider::chat_stream`] so the synchronous
/// trait method stays readable; this function is what the inner
/// async block executes.
#[allow(clippy::too_many_arguments)]
async fn open_stream(
    provider_id: String,
    base_url: String,
    api_key_ref: SecretRef,
    secret_store: Arc<dyn SecretStore>,
    http: Arc<SandboxedHttpClient>,
    extra_headers: Vec<(String, String)>,
    conv: Conversation,
    model: ModelRef,
    params: GenerationParams,
) -> Result<BoxStream<'static, Result<StreamEvent, AiError>>, AiError> {
    let messages = build_messages(&conv)?;
    let body = ChatRequest {
        model: &model.model_id,
        messages,
        stream: true,
        temperature: params.temperature,
        top_p: params.top_p,
        max_tokens: params.max_tokens,
        stop: params.stop.iter().map(String::as_str).collect(),
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
    };

    let url = build_url(&base_url);
    let request = build_request(
        &http,
        &url,
        &api_key_ref,
        &*secret_store,
        &extra_headers,
        &body,
    )?;

    let response = http
        .send(request)
        .await
        .map_err(|e| transport_to_ai_error(&provider_id, e))?;

    let status = response.status().as_u16();
    if !response.status().is_success() {
        let headers = response.headers().clone();
        let body = response.bytes().await.unwrap_or_default();
        return Err(map_status(status, &headers, &body));
    }

    let provider_id_for_stream = provider_id.clone();
    let byte_stream = response.bytes_stream();
    let event_stream = stream::unfold(
        StreamState::new(byte_stream, provider_id_for_stream),
        StreamState::pump,
    );

    Ok(event_stream.boxed())
}

/// Build the array of `{ role, content }` messages from a
/// [`Conversation`].
///
/// Rejects non-text [`Part`]s with [`AiError::InvalidRequest`] —
/// tool calls, tool results, and images flow in the next slice.
/// The optional `system_prompt` becomes a leading `system` message
/// when present.
fn build_messages(conv: &Conversation) -> Result<Vec<ChatMessage<'_>>, AiError> {
    let mut messages = Vec::with_capacity(conv.turns.len() + 1);
    if let Some(system) = conv.system_prompt.as_deref() {
        messages.push(ChatMessage {
            role: "system",
            content: system,
        });
    }
    for turn in &conv.turns {
        let role = role_to_wire(turn.role)?;
        let content = collect_text(&turn.parts)?;
        messages.push(ChatMessage { role, content });
    }
    Ok(messages)
}

fn role_to_wire(role: Role) -> Result<&'static str, AiError> {
    match role {
        Role::System => Ok("system"),
        Role::User => Ok("user"),
        Role::Assistant => Ok("assistant"),
        Role::Tool => Err(AiError::InvalidRequest(
            "tool turns are not supported in the text-streaming slice".to_string(),
        )),
    }
}

fn collect_text(parts: &[Part]) -> Result<&str, AiError> {
    // The text-only slice expects every assistant/user turn to carry
    // exactly one `Part::Text`. Multiple text parts could be
    // concatenated, but the wire format here is a single string per
    // message; rejecting shapes we cannot represent keeps the
    // boundary explicit.
    if parts.len() != 1 {
        return Err(AiError::InvalidRequest(format!(
            "this slice expects exactly one text part per turn, got {}",
            parts.len()
        )));
    }
    match &parts[0] {
        Part::Text(text) => Ok(text.as_str()),
        _ => Err(AiError::InvalidRequest(
            "this slice supports text parts only — tool calls / images land in the next slice"
                .to_string(),
        )),
    }
}

/// Compose the full request URL from the configured base URL.
///
/// Trailing slashes on the base URL are tolerated so callers can
/// pass either `https://example/v1` or `https://example/v1/`.
fn build_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    format!("{trimmed}/chat/completions")
}

/// Assemble the [`reqwest::Request`] with `Authorization`,
/// `Content-Type`, and any configured extra headers.
fn build_request<B: Serialize>(
    http: &SandboxedHttpClient,
    url: &str,
    api_key_ref: &SecretRef,
    secret_store: &dyn SecretStore,
    extra_headers: &[(String, String)],
    body: &B,
) -> Result<reqwest::Request, AiError> {
    let api_key = secret_store
        .get(api_key_ref.as_str())
        .map_err(|e| AiError::Auth(format!("secret store error: {e}")))?
        .ok_or_else(|| {
            AiError::Auth(format!(
                "no API key configured for handle {}",
                api_key_ref.as_str()
            ))
        })?;

    let mut headers = HeaderMap::new();
    let mut auth_value = HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|_| {
        AiError::Auth("API key contains characters invalid in a header".to_string())
    })?;
    auth_value.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth_value);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    for (name, value) in extra_headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AiError::InvalidRequest(format!("invalid extra header name: {name}")))?;
        let header_value = HeaderValue::from_str(value).map_err(|_| {
            AiError::InvalidRequest(format!("invalid extra header value for {name}"))
        })?;
        headers.insert(header_name, header_value);
    }

    http.client()
        .post(url)
        .headers(headers)
        .json(body)
        .build()
        .map_err(|e| AiError::Other(format!("failed to build request: {e}")))
}

/// Map a transport-level [`crate::HttpClientError`] onto the
/// categorised [`AiError`] surface.
fn transport_to_ai_error(provider_id: &str, err: crate::HttpClientError) -> AiError {
    use crate::HttpClientError;
    match err {
        HttpClientError::SandboxBlocked(reason) | HttpClientError::SandboxDenied(reason) => {
            AiError::Auth(format!(
                "sandbox refused network call to provider {provider_id}: {reason}"
            ))
        }
        HttpClientError::ApprovalTimeout(_) => {
            AiError::Network(format!("approval timed out for provider {provider_id}"))
        }
        HttpClientError::InvalidUrl(reason) => AiError::InvalidRequest(format!(
            "invalid base URL for provider {provider_id}: {reason}"
        )),
        HttpClientError::MissingHost => AiError::InvalidRequest(format!(
            "base URL for provider {provider_id} has no host component"
        )),
        HttpClientError::Transport(e) => AiError::Network(e.to_string()),
        // `Status` cannot reach this path because `SandboxedHttpClient::send`
        // does not synthesise it, but we map it conservatively anyway so a
        // future regression cannot silently down-grade a server error. The
        // `retry_after` field is the rate-limit hint Slice 8 added; preserve
        // it onto `AiError::RateLimited` when the upstream surfaced one.
        HttpClientError::Status {
            status,
            body,
            retry_after,
        } => {
            if status == 429 {
                AiError::RateLimited { retry_after }
            } else {
                AiError::ProviderError {
                    provider_id: provider_id.to_string(),
                    message: format!("HTTP {status}: {body}"),
                }
            }
        }
    }
}

/// Streaming pump: feeds bytes into the SSE parser and the SSE
/// payloads into the chat-chunk decoder, surfacing
/// [`StreamEvent`]s as they accumulate.
///
/// The pump terminates on `[DONE]`, on an upstream EOF, or on the
/// first transport error. The terminal [`StreamEvent::Done`] is
/// always synthesised exactly once unless an error short-circuits
/// the stream.
struct StreamState {
    byte_stream:
        std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    parser: SseParser,
    pending: std::collections::VecDeque<StreamEvent>,
    done_emitted: bool,
    finished: bool,
    provider_id: String,
}

impl StreamState {
    fn new<S>(byte_stream: S, provider_id: String) -> Self
    where
        S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    {
        Self {
            byte_stream: Box::pin(byte_stream),
            parser: SseParser::new(),
            pending: std::collections::VecDeque::new(),
            done_emitted: false,
            finished: false,
            provider_id,
        }
    }

    /// Async unfold step. Returns `(item, next_state)` while items
    /// are available, `None` when the stream is exhausted.
    async fn pump(mut self) -> Option<(Result<StreamEvent, AiError>, Self)> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some((Ok(event), self));
            }
            if self.finished {
                if self.done_emitted {
                    return None;
                }
                self.done_emitted = true;
                return Some((Ok(StreamEvent::Done), self));
            }

            match self.byte_stream.next().await {
                Some(Ok(chunk)) => {
                    self.parser.feed(&chunk);
                    if let Err(err) = self.drain_events() {
                        // Surface the error and stop the stream — no
                        // further events are emitted afterwards.
                        self.finished = true;
                        self.done_emitted = true;
                        return Some((Err(err), self));
                    }
                }
                Some(Err(e)) => {
                    self.finished = true;
                    self.done_emitted = true;
                    return Some((Err(AiError::Network(e.to_string())), self));
                }
                None => {
                    self.finished = true;
                }
            }
        }
    }

    /// Drain every event the parser has queued, decode JSON, and
    /// push [`StreamEvent`]s onto [`Self::pending`]. The terminal
    /// `[DONE]` event flips [`Self::finished`] so the next pump
    /// step emits [`StreamEvent::Done`] and stops.
    fn drain_events(&mut self) -> Result<(), AiError> {
        while let Some(payload) = self.parser.next_event() {
            if payload.trim() == "[DONE]" {
                self.finished = true;
                return Ok(());
            }
            let chunk: ChatStreamChunk =
                serde_json::from_str(&payload).map_err(|e| AiError::ProviderError {
                    provider_id: self.provider_id.clone(),
                    message: format!("malformed stream chunk: {e}"),
                })?;
            for choice in chunk.choices {
                if let Some(text) = choice.delta.content {
                    if !text.is_empty() {
                        self.pending.push_back(StreamEvent::TextDelta(text));
                    }
                }
            }
            if let Some(usage) = chunk.usage {
                let cached = usage
                    .prompt_tokens_details
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
                self.pending.push_back(StreamEvent::UsageReport(Usage::new(
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    cached,
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openspace_shared::ai::domain::{Conversation, Part, Role, Turn};
    use openspace_shared::id::{ChatId, TurnId};

    fn one_user_turn(text: &str) -> Conversation {
        Conversation::new(
            ChatId::new_v4(),
            None,
            vec![Turn::new(
                TurnId::new_v4(),
                Role::User,
                vec![Part::Text(text.to_string())],
                chrono::Utc::now(),
                None,
            )],
            None,
        )
    }

    #[test]
    fn build_messages_emits_system_then_turns() {
        let mut conv = one_user_turn("hi");
        conv.system_prompt = Some("be brief".to_string());
        let msgs = build_messages(&conv).expect("builds");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "be brief");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "hi");
    }

    #[test]
    fn build_messages_rejects_non_text_parts() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![Turn::new(
                TurnId::new_v4(),
                Role::User,
                vec![Part::Image {
                    mime: "image/png".to_string(),
                    data: openspace_shared::ai::domain::ImagePayload::Url("x".to_string()),
                }],
                chrono::Utc::now(),
                None,
            )],
            None,
        );
        let err = build_messages(&conv).expect_err("non-text rejected");
        assert!(matches!(err, AiError::InvalidRequest(_)), "got {err:?}");
    }

    #[test]
    fn build_messages_rejects_tool_role_in_text_slice() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![Turn::new(
                TurnId::new_v4(),
                Role::Tool,
                vec![Part::Text("result".to_string())],
                chrono::Utc::now(),
                None,
            )],
            None,
        );
        let err = build_messages(&conv).expect_err("tool role rejected");
        assert!(matches!(err, AiError::InvalidRequest(_)), "got {err:?}");
    }

    #[test]
    fn build_url_handles_trailing_slash() {
        assert_eq!(
            build_url("https://example.test/v1/"),
            "https://example.test/v1/chat/completions"
        );
        assert_eq!(
            build_url("https://example.test/v1"),
            "https://example.test/v1/chat/completions"
        );
    }
}
