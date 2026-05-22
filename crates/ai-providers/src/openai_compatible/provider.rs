//! [`OpenAiCompatibleProvider`] — adapter for the industry-standard
//! `/v1/chat/completions` wire format.
//!
//! Slice 5 covered text-only streaming and full error
//! classification. This slice (#49) layers on:
//!
//! - **Tool calling** — outgoing `tool_calls` on assistant messages
//!   and `role = "tool"` messages for results, plus incoming
//!   streamed tool-call deltas mapped onto
//!   [`StreamEvent::ToolCallDelta`]. The streaming reassembler
//!   ([`crate::ToolCallAccumulator`]) handles fragment accumulation
//!   on the consumer side; this adapter only emits well-formed
//!   delta events.
//! - **Vision input** — [`Part::Image`] content lifted into the
//!   wire-format `image_url` content block. Both `Url` and inline
//!   `Bytes` payloads are supported; inline bytes are encoded as a
//!   `data:<mime>;base64,…` URL.
//! - **Capability discovery** — [`AiProvider::list_models`] enumerates
//!   the upstream `/models` endpoint, parses each row's
//!   capability hint (when the upstream surfaces one), falls back
//!   to a heuristic table for known model families, and applies
//!   the configured `model_filter` regex before returning.
//! - **Capability-mismatch guard** — a chat call whose
//!   [`Conversation`] uses an image part against a non-vision model,
//!   or a tool-call part against a tool-less model, surfaces
//!   [`AiError::Unsupported`] before bytes leave the adapter.
//!
//! # Construction
//!
//! Adapters configure five fields:
//!
//! - `base_url` — origin including scheme and (optionally) port. The
//!   `/chat/completions` and `/models` paths are appended internally;
//!   callers that want a different path layer that on top by passing
//!   the full prefix.
//! - `api_key_ref` — handle the [`SecretStore`] resolves at request
//!   time. The provider never holds the secret value across calls.
//! - `extra_headers` — bespoke headers attached to every request
//!   (e.g. a vendor-specific organisation id).
//! - `request_id_header` — header name to receive the
//!   correlation id when the upstream advertises one. Stored
//!   verbatim; this slice does not yet thread the value into the
//!   stream output.
//! - `model_filter` — optional regex applied to the `/models` rows
//!   before they surface to the caller. A row whose `model_id`
//!   matches the regex is kept; non-matches are dropped.
//!
//! # Streaming protocol
//!
//! Every chunk on the wire arrives as an SSE `data:` event whose
//! payload is a single JSON object. The pump yields:
//!
//! - [`StreamEvent::TextDelta`] for every non-empty text content
//!   delta.
//! - [`StreamEvent::ToolCallDelta`] for every non-empty tool-call
//!   fragment. The first fragment for a given call slot carries
//!   `name = Some(_)`; subsequent fragments for the same slot carry
//!   `name = None` and append to the consumer's running arguments
//!   buffer. Consumers feed the deltas into
//!   [`crate::ToolCallAccumulator`] to reassemble the final
//!   arguments object.
//! - [`StreamEvent::UsageReport`] when the final chunk carries a
//!   usage block.
//! - [`StreamEvent::Done`] when either the upstream emits `[DONE]`
//!   or the stream closes cleanly.
//!
//! Cancellation is driven by dropping the returned `BoxStream` —
//! the underlying `reqwest::Response` body is dropped with it,
//! which closes the TCP connection. This is the same primitive the
//! sandbox client's integration test pins.
//!
//! [`Conversation`]: openspace_shared::ai::domain::Conversation
//! [`Part::Image`]: openspace_shared::ai::domain::Part::Image
//! [`SecretStore`]: openspace_shared::ai::secret::SecretStore

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use openspace_shared::ai::domain::{
    Capabilities, Conversation, GenerationParams, ModelInfo, ModelRef, Usage,
};
use openspace_shared::ai::error::AiError;
use openspace_shared::ai::provider::{AiProvider, StreamEvent, ToolCallDelta};
use openspace_shared::ai::secret::{SecretRef, SecretStore};
use regex::Regex;
use static_assertions::assert_impl_all;

use crate::client::SandboxedHttpClient;

use super::catalogue::{capabilities_for, make_model_info};
use super::error_map::map_status;
use super::request::{
    build_messages, build_models_request, build_request, build_url, check_capabilities, models_url,
};
use super::sse::SseParser;
use super::wire::{ChatRequest, ChatStreamChunk, ModelsResponse, StreamOptions};

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

    /// Configure the regex `list_models` applies to the upstream
    /// catalogue. Stored verbatim; the regex is compiled lazily
    /// inside `list_models` so a malformed pattern surfaces as
    /// [`AiError::InvalidRequest`] at the call site rather than at
    /// construction time.
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
        // Provider-level upper bound: this adapter handles vision
        // input, tool calling, and streaming for any model whose
        // catalogue row advertises them. Per-model gating happens
        // inside `chat_stream` via [`check_capabilities`].
        Capabilities::new(true, true, true)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        let provider_id = self.id.clone();
        let url = models_url(&self.base_url);
        let request = build_models_request(
            &self.http,
            &url,
            &self.api_key_ref,
            &*self.secret_store,
            &self.extra_headers,
        )?;

        let response = self
            .http
            .send(request)
            .await
            .map_err(|e| transport_to_ai_error(&provider_id, e))?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();
            return Err(map_status(status, &headers, &body));
        }

        let body = response.bytes().await.map_err(|e| AiError::ProviderError {
            provider_id: provider_id.clone(),
            message: format!("failed to read /models body: {e}"),
        })?;
        let parsed: ModelsResponse =
            serde_json::from_slice(&body).map_err(|e| AiError::ProviderError {
                provider_id: provider_id.clone(),
                message: format!("malformed /models response: {e}"),
            })?;

        let filter = match self.model_filter.as_deref() {
            Some(pattern) => Some(Regex::new(pattern).map_err(|e| {
                AiError::InvalidRequest(format!("invalid model_filter regex: {e}"))
            })?),
            None => None,
        };

        let models: Vec<ModelInfo> = parsed
            .data
            .iter()
            .filter(|entry| filter.as_ref().is_none_or(|re| re.is_match(&entry.id)))
            .map(|entry| make_model_info(&provider_id, entry))
            .collect();
        Ok(models)
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
    // Resolve the per-model capability view from the heuristic
    // catalogue and gate the call before bytes leave the adapter.
    // We deliberately stay synchronous here (no extra HTTP round
    // trip to `/models`) — the chat endpoint itself is the
    // authoritative arbiter, the heuristic table is a fast
    // pre-flight, and the dynamic verdict is still available via
    // [`AiProvider::list_models`] for callers that want it.
    let resolved_caps = capabilities_for(&model.model_id);
    let model_label = format!("{}/{}", model.provider_id, model.model_id);
    check_capabilities(&conv, resolved_caps, &model_label)?;

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
    /// `tool_calls[index].id` discovered on the first chunk for that
    /// slot. Subsequent chunks for the same slot reuse the id. The
    /// upstream wire format does not always re-send the id on every
    /// chunk; the map keeps every fragment correlated.
    tool_call_ids: std::collections::BTreeMap<u32, String>,
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
            tool_call_ids: std::collections::BTreeMap::new(),
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
                for fragment in choice.delta.tool_calls {
                    if let Some(event) = self.fragment_to_event(fragment) {
                        self.pending.push_back(event);
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

    /// Translate one wire-level `tool_calls` fragment into a
    /// [`StreamEvent::ToolCallDelta`].
    ///
    /// Returns `None` when the fragment carries neither a name nor
    /// an arguments delta — emitting an empty event would only
    /// pollute the consumer's accumulator.
    fn fragment_to_event(&mut self, fragment: super::wire::ToolCallChunk) -> Option<StreamEvent> {
        // Resolve the call id. The first fragment for a given
        // index supplies it; later fragments for the same index
        // reuse the cached value. An upstream that never sends
        // an id is non-conformant, but the consumer side still
        // cares about a stable correlation handle, so we
        // synthesise one as a last resort.
        let call_id = match (fragment.id, self.tool_call_ids.get(&fragment.index)) {
            (Some(id), _) => {
                self.tool_call_ids.insert(fragment.index, id.clone());
                id
            }
            (None, Some(cached)) => cached.clone(),
            (None, None) => {
                let synthetic = format!("call_{}", fragment.index);
                self.tool_call_ids.insert(fragment.index, synthetic.clone());
                synthetic
            }
        };
        let (name, arguments_delta) = match fragment.function {
            Some(function) => (function.name, function.arguments.unwrap_or_default()),
            None => (None, String::new()),
        };
        if name.is_none() && arguments_delta.is_empty() {
            return None;
        }
        Some(StreamEvent::ToolCallDelta(ToolCallDelta::new(
            call_id,
            name,
            arguments_delta,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openspace_shared::ai::domain::{Conversation, ImagePayload, Part, Role, Turn};
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
        assert_eq!(msgs[1].role, "user");
    }

    #[test]
    fn build_messages_renders_user_image_as_image_url_block() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![Turn::new(
                TurnId::new_v4(),
                Role::User,
                vec![
                    Part::Text("here:".to_string()),
                    Part::Image {
                        mime: "image/png".to_string(),
                        data: ImagePayload::Url("https://example.invalid/x.png".to_string()),
                    },
                ],
                chrono::Utc::now(),
                None,
            )],
            None,
        );
        let msgs = build_messages(&conv).expect("builds");
        let json = serde_json::to_string(&msgs).expect("serialise");
        assert!(json.contains(r#""type":"image_url""#));
        assert!(json.contains(r#""url":"https://example.invalid/x.png""#));
    }

    #[test]
    fn build_messages_renders_assistant_tool_call() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![Turn::new(
                TurnId::new_v4(),
                Role::Assistant,
                vec![Part::ToolCall {
                    id: "call_1".to_string(),
                    name: "search".to_string(),
                    arguments: serde_json::json!({"q": "rust"}),
                }],
                chrono::Utc::now(),
                None,
            )],
            None,
        );
        let msgs = build_messages(&conv).expect("builds");
        let json = serde_json::to_string(&msgs).expect("serialise");
        assert!(json.contains(r#""tool_calls""#));
        assert!(json.contains(r#""id":"call_1""#));
        assert!(json.contains(r#""name":"search""#));
        // Arguments serialised as an escaped JSON string per spec.
        assert!(json.contains(r#""arguments":"{\"q\":\"rust\"}""#));
    }

    #[test]
    fn build_messages_renders_tool_role_as_tool_message() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![Turn::new(
                TurnId::new_v4(),
                Role::Tool,
                vec![Part::ToolResult {
                    call_id: "call_1".to_string(),
                    content: serde_json::json!({"hits": 7}),
                    is_error: false,
                }],
                chrono::Utc::now(),
                None,
            )],
            None,
        );
        let msgs = build_messages(&conv).expect("builds");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "tool");
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_1"));
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

    // ─────────────────────────────────────────────────────────────────
    // proptest round-trip — Conversation → wire-format `messages` →
    // back through serde, asserting every Domain part is reachable
    // from the rendered envelope. The acceptance criteria pin a
    // round-trip on text + tool calls + tool results + image refs;
    // the proptest below generates random combinations of those four
    // shapes and asserts the rendered JSON contains an
    // identification marker for every one of them.
    // ─────────────────────────────────────────────────────────────────

    #[derive(Debug, Clone)]
    enum FuzzPart {
        Text(String),
        ToolCall { id: String, name: String },
        ToolResult { id: String, body: String },
        ImageUrl(String),
    }

    fn fuzz_part_strategy() -> impl proptest::strategy::Strategy<Value = FuzzPart> {
        use proptest::prelude::*;
        prop_oneof![
            "[a-z ]{1,16}".prop_map(FuzzPart::Text),
            ("[a-z]{4,8}", "[a-z_]{3,12}").prop_map(|(id, name)| FuzzPart::ToolCall { id, name }),
            ("[a-z]{4,8}", "[a-z0-9 ]{1,16}")
                .prop_map(|(id, body)| FuzzPart::ToolResult { id, body }),
            "https://[a-z]{3,8}\\.example/[a-z]{3,8}\\.png".prop_map(FuzzPart::ImageUrl),
        ]
    }

    fn build_conversation(parts: &[FuzzPart]) -> Conversation {
        // Distribute parts across canonical role buckets so the
        // emitted conversation respects the wire format's own
        // structural rules (tool results live on tool turns,
        // tool calls on assistant turns, images on user turns).
        let mut user_parts = Vec::new();
        let mut assistant_parts = Vec::new();
        let mut tool_parts = Vec::new();
        for part in parts {
            match part {
                FuzzPart::Text(t) => user_parts.push(Part::Text(t.clone())),
                FuzzPart::ToolCall { id, name } => assistant_parts.push(Part::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: serde_json::json!({"arg": id}),
                }),
                FuzzPart::ToolResult { id, body } => tool_parts.push(Part::ToolResult {
                    call_id: id.clone(),
                    content: serde_json::Value::String(body.clone()),
                    is_error: false,
                }),
                FuzzPart::ImageUrl(url) => user_parts.push(Part::Image {
                    mime: "image/png".to_string(),
                    data: ImagePayload::Url(url.clone()),
                }),
            }
        }
        let mut turns = Vec::new();
        if !user_parts.is_empty() {
            turns.push(Turn::new(
                TurnId::new_v4(),
                Role::User,
                user_parts,
                chrono::Utc::now(),
                None,
            ));
        }
        if !assistant_parts.is_empty() {
            turns.push(Turn::new(
                TurnId::new_v4(),
                Role::Assistant,
                assistant_parts,
                chrono::Utc::now(),
                None,
            ));
        }
        if !tool_parts.is_empty() {
            turns.push(Turn::new(
                TurnId::new_v4(),
                Role::Tool,
                tool_parts,
                chrono::Utc::now(),
                None,
            ));
        }
        Conversation::new(ChatId::new_v4(), None, turns, None)
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(64))]

        #[test]
        fn proptest_conversation_round_trips_through_wire_format(
            parts in proptest::collection::vec(fuzz_part_strategy(), 1..6)
        ) {
            let conv = build_conversation(&parts);
            let messages = build_messages(&conv).expect("builds");
            let json = serde_json::to_string(&messages).expect("serialise");
            for part in &parts {
                match part {
                    FuzzPart::Text(t) => proptest::prop_assert!(json.contains(t)),
                    FuzzPart::ToolCall { id, name } => {
                        proptest::prop_assert!(json.contains(id));
                        proptest::prop_assert!(json.contains(name));
                    }
                    FuzzPart::ToolResult { id, body } => {
                        proptest::prop_assert!(json.contains(id));
                        proptest::prop_assert!(json.contains(body));
                    }
                    FuzzPart::ImageUrl(url) => proptest::prop_assert!(json.contains(url)),
                }
            }
        }
    }
}
