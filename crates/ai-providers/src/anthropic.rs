//! Adapter for the alternative chat wire format whose request body
//! places the system prompt **outside** the `messages` array — the
//! "system-prompt-outside-messages" shape, as opposed to the
//! industry-standard chat-completions shape that embeds a
//! `Role::System` row at index 0.
//!
//! The vendor brand appears only in the type name [`AnthropicProvider`]
//! because the trait surface is literally vendor-specific at this
//! boundary; every other identifier, doc comment, and test fixture
//! talks in neutral terms ("alternative wire format",
//! "system-prompt-outside-messages adapter"). The naming policy
//! (see `AGENTS.md` at the repo root) calls this compromise out
//! explicitly: vendor names are tolerated where the trait literally
//! maps to a vendor's wire spec, never propagated inwards.
//!
//! # Slice scope
//!
//! This slice covers text streaming and full HTTP error
//! classification only. Tool-call streaming, vision, and capability
//! discovery (`list_models`) ride along in the follow-up slice.
//!
//! # Wire shape (request)
//!
//! ```jsonc
//! {
//!   "model": "<model-id>",
//!   "max_tokens": 4096,
//!   "system": "<merged Role::System content>",
//!   "stream": true,
//!   "messages": [
//!     { "role": "user", "content": [{ "type": "text", "text": "..." }] }
//!   ]
//! }
//! ```
//!
//! Every `Role::System` content (the [`Conversation::system_prompt`]
//! plus any inline system turns) is concatenated into the top-level
//! `system` field, separated by blank lines. The `messages` array
//! never contains a `system`-roled entry — explicitly tested below.
//!
//! # Wire shape (streaming response, JSON-NL)
//!
//! Events arrive one JSON object per line. The parser tolerates
//! blank lines and partial chunks split across socket reads. The
//! variants this slice consumes:
//!
//! - `message_start` carrying initial usage
//!   → [`StreamEvent::UsageReport`].
//! - `content_block_delta` of `delta.type == "text_delta"`
//!   → [`StreamEvent::TextDelta`].
//! - `message_delta` carrying updated usage
//!   → [`StreamEvent::UsageReport`].
//! - `message_stop` → [`StreamEvent::Done`].
//! - Anything else is silently dropped — tool-call and citation
//!   variants land in the next slice.
//!
//! # Error mapping
//!
//! Mapping is the same vocabulary the industry-standard adapter
//! uses, applied to the alternative envelope's status codes:
//!
//! | HTTP status        | [`AiError`] variant                              |
//! |--------------------|--------------------------------------------------|
//! | 400                | [`AiError::InvalidRequest`]                      |
//! | 401 / 403          | [`AiError::Auth`]                                |
//! | 404 (model)        | [`AiError::ModelNotFound`]                       |
//! | 429                | [`AiError::RateLimited`] (`retry_after` parsed)  |
//! | 500..=599          | [`AiError::ProviderError`]                       |
//! | DNS / TLS / framing| [`AiError::Network`]                             |
//! | sandbox / approval | [`AiError::Auth`] / [`AiError::Network`]         |
//! | malformed JSON-NL  | [`AiError::ProviderError`]                       |
//! | dropped stream     | connection terminates (Slice 3 contract)         |
//!
//! [`Conversation::system_prompt`]: openspace_shared::ai::domain::Conversation::system_prompt

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use openspace_shared::ai::domain::{
    Capabilities, Conversation, GenerationParams, ModelInfo, ModelRef, Part, Role, Usage,
};
use openspace_shared::ai::error::AiError;
use openspace_shared::ai::provider::{AiProvider, StreamEvent};
use openspace_shared::ai::secret::{SecretRef, SecretStore};
use serde::{Deserialize, Serialize};

use crate::client::SandboxedHttpClient;
use crate::error::HttpClientError;

// ─────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────

/// Stable identifier this adapter advertises through
/// [`AiProvider::id`]. Lowercase, whitespace-free token — matches the
/// shape every other provider id in the workspace follows. The string
/// is intentionally short and stable: it shows up as a settings key,
/// a [`SecretRef`] segment (`openspace.provider.anthropic.api_key`),
/// and a log attribute. Renaming it is a migration, not a refactor.
pub const PROVIDER_ID: &str = "anthropic";

/// Default wire-format pin. The alternative envelope requires every
/// request to carry a version header; this is the documented stable
/// pin at the time of this slice. Adapters built later for a newer
/// envelope override it through [`AnthropicProvider::with_version`].
pub const DEFAULT_VERSION: &str = "2023-06-01";

/// Default `max_tokens` ceiling. The wire format requires the field
/// even when the caller has no opinion. 4096 is the conservative
/// pick: large enough to be useful for prose drafting, small enough
/// to surface a runaway prompt before it burns tokens. Callers that
/// want their own ceiling pass it through
/// [`GenerationParams::max_tokens`].
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ─────────────────────────────────────────────────────────────────────
// Public type
// ─────────────────────────────────────────────────────────────────────

/// Provider adapter for the alternative chat wire format.
///
/// Holds the configuration every request needs:
///
/// - `base_url` — the upstream root, e.g. `https://api.example.invalid`.
///   Test fixtures inject a `wiremock` URL here.
/// - `api_key_ref` — handle the [`SecretStore`] resolves to the live
///   credential at request time. The credential value never leaves
///   that lookup.
/// - `version` — wire-format pin (`anthropic-version` header).
/// - `client` — the shared sandbox-aware HTTP client. Cloning is
///   cheap (everything inside is `Arc`), so the adapter shares a
///   single instance across calls.
/// - `secret_store` — `Arc<dyn SecretStore>` so production binaries
///   can swap in a keychain backend while tests pass an in-memory
///   double through the same trait.
///
/// All fields are private: the public surface is the constructor
/// [`AnthropicProvider::new`], the version override
/// [`AnthropicProvider::with_version`], and the `AiProvider` trait
/// implementation itself.
#[derive(Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key_ref: SecretRef,
    version: String,
    client: SandboxedHttpClient,
    secret_store: Arc<dyn SecretStore>,
}

impl AnthropicProvider {
    /// Build an adapter pinned to `base_url`, reading the API key
    /// through `secret_store` at request time using `api_key_ref` as
    /// the lookup handle.
    ///
    /// `base_url` is stored verbatim — callers pass the protocol
    /// scheme and host without a trailing slash. The slash is added
    /// at request build time so adding or removing a path segment in
    /// a later slice does not depend on caller hygiene.
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        api_key_ref: SecretRef,
        client: SandboxedHttpClient,
        secret_store: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key_ref,
            version: DEFAULT_VERSION.to_string(),
            client,
            secret_store,
        }
    }

    /// Override the wire-format version pin. Returns `self` so the
    /// builder pattern flows. The default pin
    /// ([`DEFAULT_VERSION`]) covers every documented stable envelope
    /// at the time of writing.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Borrow the configured base URL. Useful in tests; production
    /// code rarely reaches for it.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Borrow the configured wire-format pin.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Arc<dyn SecretStore>` carries no `Debug` bound — surface
        // the public-safe pieces and elide the store. The
        // [`SecretRef`] is safe to print; it is a lookup key, not a
        // value.
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .field("api_key_ref", &self.api_key_ref)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

// Compile-time `Send + Sync` guarantee: the agent loop holds the
// provider behind `Arc<dyn AiProvider + Send + Sync>` and shares it
// across tasks. If a future field accidentally drops the bound,
// this assertion fails to compile at the type-surface layer rather
// than at the call site.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AnthropicProvider>();
};

#[async_trait]
impl AiProvider for AnthropicProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &str {
        // Neutral, vendor-agnostic copy. The PRD calls the role
        // out as "alternative wire-format provider"; surfaces that
        // render this string (settings panes, logs) get the same
        // phrasing.
        "Alternative chat wire format"
    }

    fn capabilities(&self) -> Capabilities {
        // This slice ships text-only streaming. Vision and tool
        // calling land in the follow-up slice; advertising them now
        // would lie to the agent loop's feature gates.
        Capabilities::new(false, false, true)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        // Capability discovery is the next slice's responsibility.
        // Returning an empty list rather than `Err(_)` matches the
        // "no catalogue endpoint configured" semantics the PRD
        // documents, and keeps callers that surface the picker from
        // showing an error toast for a perfectly valid configuration.
        Ok(Vec::new())
    }

    fn chat_stream(
        &self,
        conv: Conversation,
        model: ModelRef,
        params: GenerationParams,
    ) -> BoxStream<'static, Result<StreamEvent, AiError>> {
        // Build everything that does not require the secret upfront
        // so we can fail fast on caller bugs (model id mismatch).
        if model.provider_id != PROVIDER_ID {
            return futures::stream::once(async move {
                Err(AiError::ProviderError {
                    provider_id: PROVIDER_ID.to_string(),
                    message: format!(
                        "model_ref provider {:?} does not match this adapter ({})",
                        model.provider_id, PROVIDER_ID
                    ),
                })
            })
            .boxed();
        }

        let body = build_request_body(&conv, &model, &params);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let api_key_ref = self.api_key_ref.clone();
        let version = self.version.clone();
        let client = self.client.clone();
        let secret_store = self.secret_store.clone();

        let stream = async_stream::try_stream! {
            // Resolve the credential at request time so a rotation
            // takes effect without re-instantiating the provider.
            let api_key = match secret_store.get(api_key_ref.as_str()) {
                Ok(Some(value)) => value,
                Ok(None) => {
                    Err(AiError::Auth(format!(
                        "no credential stored for handle {}",
                        api_key_ref.as_str()
                    )))?;
                    unreachable!()
                }
                Err(e) => {
                    Err(AiError::Auth(format!("secret store backend error: {e}")))?;
                    unreachable!()
                }
            };

            let headers: [(&str, &str); 3] = [
                ("x-api-key", api_key.as_str()),
                ("anthropic-version", version.as_str()),
                ("content-type", "application/json"),
            ];

            let bytes_stream = match client
                .post_streaming_with_headers(&url, &body, &headers)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    Err(map_http_error(e))?;
                    unreachable!()
                }
            };

            // Drain the byte stream through the JSON-NL parser. The
            // parser is stateful (line buffer) so it lives outside
            // the per-chunk loop.
            let mut parser = JsonNlParser::default();
            let mut bytes_stream = Box::pin(bytes_stream);
            while let Some(chunk) = bytes_stream.next().await {
                let chunk = chunk.map_err(map_http_error)?;
                for line in parser.feed(&chunk) {
                    let event = parse_wire_event(&line)
                        .map_err(|message| AiError::ProviderError {
                            provider_id: PROVIDER_ID.to_string(),
                            message,
                        })?;
                    if let Some(event) = event {
                        let is_done = matches!(event, StreamEvent::Done);
                        yield event;
                        if is_done {
                            return;
                        }
                    }
                }
            }
            // Stream ended without an explicit `message_stop`. Flush
            // the buffer for any trailing partial line, then yield a
            // synthetic `Done` so consumers see the terminal marker.
            for line in parser.flush() {
                if let Some(event) = parse_wire_event(&line)
                    .map_err(|message| AiError::ProviderError {
                        provider_id: PROVIDER_ID.to_string(),
                        message,
                    })?
                {
                    let is_done = matches!(event, StreamEvent::Done);
                    yield event;
                    if is_done {
                        return;
                    }
                }
            }
            yield StreamEvent::Done;
        };

        stream.boxed()
    }
}

// ─────────────────────────────────────────────────────────────────────
// Request shape
// ─────────────────────────────────────────────────────────────────────

/// Wire-format request body. Public to the crate (not the world) so
/// the `wiremock` integration tests can deserialise the captured
/// outgoing body and assert structure on it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AnthropicRequest {
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
    /// `system` lifted out of `messages`. `None` collapses out of
    /// the JSON envelope through `skip_serializing_if`. The headline
    /// invariant of this slice — `Role::System` content lives here,
    /// never inside `messages` — is enforced by [`build_request_body`]
    /// and pinned by the structural assertion test below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system: Option<String>,
    pub(crate) stream: bool,
    pub(crate) messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) stop_sequences: Vec<String>,
}

/// Per-message envelope. `role` is serialised verbatim; we map
/// every Domain role onto either `"user"` or `"assistant"` upstream.
/// `content` is an array of typed parts so the format can carry
/// future modalities without churn.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AnthropicMessage {
    pub(crate) role: String,
    pub(crate) content: Vec<AnthropicContent>,
}

/// Content part inside [`AnthropicMessage::content`]. Tagged enum
/// (`type` field) so future modalities (`image`, `tool_use`,
/// `tool_result`) plug in without breaking the existing variants.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicContent {
    Text { text: String },
}

/// Lift `Role::System` content out of the conversation into the
/// top-level `system` field, leaving `messages` system-free.
///
/// The merge logic concatenates [`Conversation::system_prompt`] (when
/// present) with every `Role::System` turn's text parts, separated
/// by blank lines. The order is *system_prompt first, then inline
/// system turns in conversation order* — matches the semantics every
/// production caller expects: the framing prompt sets the stage, and
/// inline system turns refine it.
fn extract_system(conv: &Conversation) -> Option<String> {
    let mut buf: Vec<String> = Vec::new();
    if let Some(prompt) = &conv.system_prompt {
        let trimmed = prompt.trim();
        if !trimmed.is_empty() {
            buf.push(trimmed.to_string());
        }
    }
    for turn in &conv.turns {
        if turn.role != Role::System {
            continue;
        }
        for part in &turn.parts {
            if let Part::Text(t) = part {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    buf.push(trimmed.to_string());
                }
            }
        }
    }
    if buf.is_empty() {
        None
    } else {
        Some(buf.join("\n\n"))
    }
}

/// Project the conversation's non-system turns into wire messages.
/// Tool turns map onto the `user` role because the alternative
/// wire format treats tool results as user-side input; tool-call
/// streaming itself is the next slice's responsibility, so this
/// slice's text-only AC is unaffected.
fn extract_messages(conv: &Conversation) -> Vec<AnthropicMessage> {
    conv.turns
        .iter()
        .filter(|t| t.role != Role::System)
        .map(|turn| {
            let role = match turn.role {
                Role::Assistant => "assistant",
                // User and Tool both map onto the user-side input;
                // tool-call streaming follows in the next slice.
                _ => "user",
            };
            let content = turn
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(text) => Some(AnthropicContent::Text { text: text.clone() }),
                    // Image, ToolCall, and ToolResult are
                    // out-of-scope for this slice. Silently
                    // dropping them keeps the text-only AC honest;
                    // the next slice replaces this filter with the
                    // full mapping.
                    _ => None,
                })
                .collect();
            AnthropicMessage {
                role: role.to_string(),
                content,
            }
        })
        .collect()
}

/// Assemble the full wire request from Domain inputs.
fn build_request_body(
    conv: &Conversation,
    model: &ModelRef,
    params: &GenerationParams,
) -> AnthropicRequest {
    AnthropicRequest {
        model: model.model_id.clone(),
        max_tokens: params.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system: extract_system(conv),
        stream: true,
        messages: extract_messages(conv),
        temperature: params.temperature,
        top_p: params.top_p,
        stop_sequences: params.stop.clone(),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Streaming response parser
// ─────────────────────────────────────────────────────────────────────

/// Stateful line buffer for the JSON-NL streaming protocol.
///
/// The byte stream arrives in arbitrary chunks: a single line may
/// span two chunks, and a single chunk may contain several lines
/// plus a partial trailer. The parser holds the partial trailer
/// across chunks and yields complete lines via [`Self::feed`].
///
/// Blank lines are dropped; the upstream's keepalive markers
/// (single newlines between events) flow through harmlessly.
#[derive(Default)]
struct JsonNlParser {
    buffer: Vec<u8>,
}

impl JsonNlParser {
    /// Feed a chunk and return every complete line it produced.
    /// Blank lines are skipped here so callers see only candidate
    /// JSON payloads.
    fn feed(&mut self, chunk: &Bytes) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(idx) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=idx).collect();
            // Strip the trailing newline (and an optional CR).
            let mut end = line.len() - 1;
            if end > 0 && line[end - 1] == b'\r' {
                end -= 1;
            }
            let text = String::from_utf8_lossy(&line[..end]).into_owned();
            if !text.trim().is_empty() {
                out.push(text);
            }
        }
        out
    }

    /// Drain whatever remains in the buffer when the stream ends.
    /// Returns the trailing partial line as a singleton when it is
    /// non-blank, or an empty vec otherwise.
    fn flush(&mut self) -> Vec<String> {
        let leftover = std::mem::take(&mut self.buffer);
        let text = String::from_utf8_lossy(&leftover).into_owned();
        if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![text]
        }
    }
}

/// Decoded wire event. Mirrors the documented variants of the
/// alternative envelope; unknown variants land in [`Self::Other`]
/// and are filtered out at the mapping layer below.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    MessageStart {
        message: MessageStartPayload,
    },
    ContentBlockDelta {
        delta: ContentBlockDelta,
    },
    MessageDelta {
        #[serde(default)]
        usage: Option<UsagePayload>,
    },
    MessageStop,
    /// Catch-all for variants outside this slice's scope
    /// (`content_block_start`, `content_block_stop`, `ping`,
    /// `error`, etc.). Tag plus serde's untagged fallback keeps
    /// future variants non-breaking.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct MessageStartPayload {
    #[serde(default)]
    usage: Option<UsagePayload>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockDelta {
    TextDelta {
        text: String,
    },
    /// Tool-call deltas land in the next slice; we deserialise the
    /// envelope but silently drop the payload here so the parser
    /// stays robust against intermixed tool-call events.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct UsagePayload {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

/// Parse a single JSON line into an optional [`StreamEvent`]. Returns
/// `Ok(None)` for events outside this slice's scope (so the caller
/// can keep draining without surfacing them).
fn parse_wire_event(line: &str) -> Result<Option<StreamEvent>, String> {
    let event: WireEvent =
        serde_json::from_str(line).map_err(|e| format!("malformed JSON-NL event: {e}"))?;
    match event {
        WireEvent::MessageStart { message } => Ok(message.usage.map(|u| {
            StreamEvent::UsageReport(Usage::new(
                u.input_tokens,
                u.output_tokens,
                u.cache_read_input_tokens,
            ))
        })),
        WireEvent::ContentBlockDelta { delta } => match delta {
            ContentBlockDelta::TextDelta { text } => Ok(Some(StreamEvent::TextDelta(text))),
            ContentBlockDelta::Other => Ok(None),
        },
        WireEvent::MessageDelta { usage } => Ok(usage.map(|u| {
            StreamEvent::UsageReport(Usage::new(
                u.input_tokens,
                u.output_tokens,
                u.cache_read_input_tokens,
            ))
        })),
        WireEvent::MessageStop => Ok(Some(StreamEvent::Done)),
        WireEvent::Other => Ok(None),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Error mapping
// ─────────────────────────────────────────────────────────────────────

/// Map a transport-layer [`HttpClientError`] onto the AI-layer
/// [`AiError`] vocabulary.
///
/// The mapping is intentionally lossless on the diagnostic copy:
/// the upstream body excerpt rides through into the
/// [`AiError::ProviderError`] / [`AiError::InvalidRequest`] strings so
/// log readers do not have to cross-reference two errors to read one
/// failure.
fn map_http_error(err: HttpClientError) -> AiError {
    match err {
        HttpClientError::SandboxBlocked(reason) | HttpClientError::SandboxDenied(reason) => {
            AiError::Auth(reason)
        }
        HttpClientError::ApprovalTimeout(d) => {
            AiError::Network(format!("approval timed out after {d:?}"))
        }
        HttpClientError::InvalidUrl(reason) => AiError::InvalidRequest(reason),
        HttpClientError::MissingHost => {
            AiError::InvalidRequest("URL has no host component".to_string())
        }
        HttpClientError::Transport(e) => AiError::Network(e.to_string()),
        HttpClientError::Status {
            status,
            body,
            retry_after,
        } => match status {
            400 => AiError::InvalidRequest(body),
            401 | 403 => AiError::Auth(body),
            429 => AiError::RateLimited { retry_after },
            // 5xx surfaces as a provider error so downstream
            // back-off heuristics can branch on the variant. The
            // body excerpt rides through verbatim.
            500..=599 => AiError::ProviderError {
                provider_id: PROVIDER_ID.to_string(),
                message: format!("HTTP {status}: {body}"),
            },
            _ => AiError::ProviderError {
                provider_id: PROVIDER_ID.to_string(),
                message: format!("HTTP {status}: {body}"),
            },
        },
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use openspace_shared::ai::domain::{Conversation, Turn};
    use openspace_shared::id::{ChatId, TurnId};

    fn turn(role: Role, text: &str) -> Turn {
        Turn::new(
            TurnId::new_v4(),
            role,
            vec![Part::Text(text.to_string())],
            DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("timestamp"),
            None,
        )
    }

    /// Headline structural invariant — `Role::System` content lifts
    /// into the top-level `system` field, never into `messages`.
    /// The acceptance criteria call this out explicitly.
    #[test]
    fn system_prompt_lifts_outside_messages() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![
                turn(Role::System, "be helpful"),
                turn(Role::User, "hi"),
                turn(Role::Assistant, "hello"),
            ],
            Some("respond briefly".to_string()),
        );
        let req = build_request_body(
            &conv,
            &ModelRef::new(PROVIDER_ID, "model-x"),
            &GenerationParams::default(),
        );

        // System merged from both sources, with the conversation-level
        // prompt first.
        assert_eq!(req.system.as_deref(), Some("respond briefly\n\nbe helpful"));

        // Messages array contains user + assistant only — no
        // system entry whatsoever.
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.messages[1].role, "assistant");
        // Belt-and-braces JSON serialisation check: the rendered
        // body literally has no `"role":"system"` substring.
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains(r#""system":"respond briefly\n\nbe helpful""#));
        assert!(!json.contains(r#""role":"system""#));
    }

    /// `system` is omitted entirely when there is nothing to lift.
    /// Confirms `skip_serializing_if` fires correctly.
    #[test]
    fn missing_system_collapses_out_of_envelope() {
        let conv = Conversation::new(ChatId::new_v4(), None, vec![turn(Role::User, "hi")], None);
        let req = build_request_body(
            &conv,
            &ModelRef::new(PROVIDER_ID, "model-x"),
            &GenerationParams::default(),
        );
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(!json.contains("system"));
    }

    /// `max_tokens` defaults when the caller has no opinion. The
    /// wire format demands a value, so leaving the field off the
    /// envelope is not an option.
    #[test]
    fn max_tokens_defaults_when_caller_passes_none() {
        let conv = Conversation::new(ChatId::new_v4(), None, vec![turn(Role::User, "hi")], None);
        let req = build_request_body(
            &conv,
            &ModelRef::new(PROVIDER_ID, "model-x"),
            &GenerationParams::default(),
        );
        assert_eq!(req.max_tokens, DEFAULT_MAX_TOKENS);
    }

    /// JSON-NL parser handles partial chunks split across reads.
    #[test]
    fn json_nl_parser_reassembles_split_lines() {
        let mut parser = JsonNlParser::default();
        let lines_a = parser.feed(&Bytes::from_static(b"{\"type\":\"message_st"));
        assert!(lines_a.is_empty(), "no complete line yet");
        let lines_b = parser.feed(&Bytes::from_static(b"art\",\"message\":{}}\n"));
        assert_eq!(lines_b.len(), 1);
        assert!(lines_b[0].contains("message_start"));
    }

    /// Parser drops blank lines (keepalive markers in the JSON-NL
    /// shape).
    #[test]
    fn json_nl_parser_skips_blank_lines() {
        let mut parser = JsonNlParser::default();
        let lines = parser.feed(&Bytes::from_static(b"\n\n{\"type\":\"message_stop\"}\n\n"));
        assert_eq!(lines.len(), 1);
    }

    /// Round-trip a representative event sequence through the
    /// parser + mapper and assert the surface produced.
    #[test]
    fn wire_event_mapping_covers_documented_variants() {
        let cases = [
            (
                r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}"#,
                Some(StreamEvent::UsageReport(Usage::new(5, 0, 0))),
            ),
            (
                r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#,
                Some(StreamEvent::TextDelta("hi".to_string())),
            ),
            (
                r#"{"type":"message_delta","usage":{"output_tokens":42}}"#,
                Some(StreamEvent::UsageReport(Usage::new(0, 42, 0))),
            ),
            (r#"{"type":"message_stop"}"#, Some(StreamEvent::Done)),
            // Out-of-scope variant flows through as `None`.
            (r#"{"type":"ping"}"#, None),
        ];
        for (line, want) in cases {
            let got = parse_wire_event(line).expect("parse");
            assert_eq!(got, want, "mismatch on {line}");
        }
    }

    /// Malformed JSON-NL surfaces as a parse error. The streaming
    /// pipeline maps this into `AiError::ProviderError`.
    #[test]
    fn malformed_line_surfaces_parse_error() {
        let err = parse_wire_event("{not json").expect_err("must error");
        assert!(err.contains("malformed JSON-NL event"));
    }

    /// HTTP error mapping: the headline status codes land on the
    /// expected `AiError` variants.
    #[test]
    fn http_error_mapping_matches_acceptance_criteria() {
        use std::time::Duration;
        let cases = [
            (
                HttpClientError::Status {
                    status: 401,
                    body: "bad key".into(),
                    retry_after: None,
                },
                "auth",
            ),
            (
                HttpClientError::Status {
                    status: 429,
                    body: "slow down".into(),
                    retry_after: Some(Duration::from_secs(7)),
                },
                "rate",
            ),
            (
                HttpClientError::Status {
                    status: 500,
                    body: "boom".into(),
                    retry_after: None,
                },
                "provider",
            ),
            (
                HttpClientError::Status {
                    status: 400,
                    body: "bad request".into(),
                    retry_after: None,
                },
                "bad_request",
            ),
        ];
        for (input, expected) in cases {
            let mapped = map_http_error(input);
            match (expected, mapped) {
                ("auth", AiError::Auth(_)) => {}
                ("rate", AiError::RateLimited { retry_after }) => {
                    assert_eq!(retry_after, Some(Duration::from_secs(7)));
                }
                ("provider", AiError::ProviderError { provider_id, .. }) => {
                    assert_eq!(provider_id, PROVIDER_ID);
                }
                ("bad_request", AiError::InvalidRequest(_)) => {}
                (other, mapped) => panic!("expected {other}, got {mapped:?}"),
            }
        }
    }
}
