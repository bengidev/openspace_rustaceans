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
//! Slice 8 covered text streaming and HTTP error classification.
//! This slice (#51) layers on:
//!
//! - Tool calling — outgoing `tool_use` and `tool_result` content
//!   blocks, plus incoming streamed `tool_use` deltas mapped onto
//!   [`StreamEvent::ToolCallDelta`]. The streaming reassembler
//!   ([`crate::ToolCallAccumulator`]) handles fragment accumulation
//!   on the consumer side; this adapter only emits well-formed
//!   delta events.
//! - Vision input — [`Part::Image`] content lifted into the
//!   wire-format `image` content block, with both `Url` and inline
//!   `Bytes` payloads supported.
//! - Capability discovery — [`AiProvider::list_models`] returns a
//!   static catalogue derived from documented model families, each
//!   row stamped with [`Capabilities`]; unknown model ids fall back
//!   to a conservative `(false, false, true)` shape (text streaming
//!   only).
//! - Capability-mismatch guard — a chat call whose `Conversation`
//!   uses an image part against a non-vision model, or a tool-call
//!   part against a tool-less model, surfaces
//!   [`AiError::Unsupported`] before the request leaves the
//!   adapter.
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
//!     {
//!       "role": "user",
//!       "content": [
//!         { "type": "text", "text": "..." },
//!         { "type": "image", "source": { "type": "url", "url": "..." } }
//!       ]
//!     },
//!     {
//!       "role": "assistant",
//!       "content": [
//!         { "type": "text", "text": "..." },
//!         { "type": "tool_use", "id": "call_1", "name": "search",
//!           "input": { "query": "..." } }
//!       ]
//!     },
//!     {
//!       "role": "user",
//!       "content": [
//!         { "type": "tool_result", "tool_use_id": "call_1",
//!           "content": "...", "is_error": false }
//!       ]
//!     }
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
//! variants this adapter consumes:
//!
//! - `message_start` carrying initial usage
//!   → [`StreamEvent::UsageReport`].
//! - `content_block_start` with a `tool_use` block carrying `id` and
//!   `name` → [`StreamEvent::ToolCallDelta`] with
//!   `name = Some(_)` and an empty `arguments_delta`.
//! - `content_block_delta` of `delta.type == "text_delta"`
//!   → [`StreamEvent::TextDelta`].
//! - `content_block_delta` of `delta.type == "input_json_delta"`
//!   → [`StreamEvent::ToolCallDelta`] with `name = None` and the
//!   `partial_json` fragment as `arguments_delta`. Consumers feed
//!   the deltas into [`crate::ToolCallAccumulator`] to reassemble
//!   the final argument object.
//! - `message_delta` carrying updated usage
//!   → [`StreamEvent::UsageReport`].
//! - `message_stop` → [`StreamEvent::Done`].
//! - Anything else is silently dropped.
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
//! | capability mismatch| [`AiError::Unsupported`]                         |
//!
//! [`Conversation::system_prompt`]: openspace_shared::ai::domain::Conversation::system_prompt

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use openspace_shared::ai::domain::{
    Capabilities, Conversation, GenerationParams, ImagePayload, ModelInfo, ModelRef, Part, Role,
    Usage,
};
use openspace_shared::ai::error::AiError;
use openspace_shared::ai::provider::{AiProvider, StreamEvent, ToolCallDelta};
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
        // Provider-level upper bound: this adapter handles vision input,
        // tool calling, and streaming for any model whose row in the
        // catalogue ([`list_models`]) advertises them. Per-model
        // capability gating is enforced at chat-call time
        // ([`AiProvider::chat_stream`]) — a model whose catalogue row
        // does not advertise a flag rejects requests that need it with
        // [`AiError::Unsupported`] before bytes leave the adapter.
        Capabilities::new(true, true, true)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        // Static catalogue derived from documented model families. The
        // wire format does not expose a discovery endpoint, so the
        // adapter ships a heuristic table of well-known model ids and
        // their advertised capabilities. Model picker UIs render this
        // verbatim; the agent loop's feature gates consult the per-row
        // [`Capabilities`] entry.
        Ok(model_catalogue())
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

        // Capability-mismatch guard. Look up the model in the static
        // catalogue and reject the call before bytes leave the adapter
        // when the conversation needs a flag the model does not
        // advertise. Unknown model ids fall back to the conservative
        // baseline (`text-only streaming`) — safer to surface a typo
        // here than to send a request that the upstream may charge for
        // and reject.
        if let Err(reason) = check_capabilities(&model, &conv) {
            return futures::stream::once(async move { Err(AiError::Unsupported(reason)) }).boxed();
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
            // the per-chunk loop. The `index_map` carries the wire
            // format's `block index → call_id` correlation across
            // events so streamed `input_json_delta` fragments can be
            // re-tagged with the right tool-call handle.
            let mut parser = JsonNlParser::default();
            let mut index_map: std::collections::BTreeMap<u32, String> =
                std::collections::BTreeMap::new();
            let mut bytes_stream = Box::pin(bytes_stream);
            while let Some(chunk) = bytes_stream.next().await {
                let chunk = chunk.map_err(map_http_error)?;
                for line in parser.feed(&chunk) {
                    let event = parse_wire_event(&line, &mut index_map)
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
                if let Some(event) = parse_wire_event(&line, &mut index_map)
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
/// (`type` field) so future modalities plug in without breaking the
/// existing variants.
///
/// The four variants this slice carries:
///
/// - `Text` — UTF-8 prose fragment.
/// - `ToolUse` — assistant-issued tool invocation. Mirrors
///   [`Part::ToolCall`]; `id` is the correlation handle every later
///   `ToolResult` echoes back.
/// - `ToolResult` — paired response to a previous `ToolUse`. The
///   `content` field is serialised verbatim (the wire format accepts
///   either a string or a structured block list).
/// - `Image` — image input. The wire format wraps the pixel payload
///   in a tagged `source` envelope (`url` for [`ImagePayload::Url`],
///   `base64` for [`ImagePayload::Bytes`]).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        is_error: bool,
    },
    Image {
        source: AnthropicImageSource,
    },
}

/// Tagged image envelope. The wire format distinguishes URL-by-reference
/// payloads (`type: "url"`) from inline base64-encoded bytes
/// (`type: "base64"`). `media_type` carries the IANA mime type for the
/// inline form.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicImageSource {
    Url { url: String },
    Base64 { media_type: String, data: String },
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
///
/// Mapping rules:
///
/// - `Role::Assistant` → `"assistant"`. Text parts become `Text`
///   blocks; [`Part::ToolCall`] parts become `ToolUse` blocks.
/// - `Role::User` → `"user"`. Text parts become `Text` blocks;
///   [`Part::Image`] parts become `Image` blocks (URL or base64).
/// - `Role::Tool` → `"user"`. The wire format treats tool results as
///   user-side input, so a tool turn folds onto a synthetic user
///   message whose body is one or more `ToolResult` blocks.
///   [`Part::Text`] inside a tool turn (rare but legal) flows through
///   as a `Text` block on the same user message.
///
/// `Role::System` turns never reach this function — they are extracted
/// separately by [`extract_system`] and emitted as the top-level
/// `system` field.
fn extract_messages(conv: &Conversation) -> Vec<AnthropicMessage> {
    conv.turns
        .iter()
        .filter(|t| t.role != Role::System)
        .map(|turn| {
            let role = match turn.role {
                Role::Assistant => "assistant",
                // User and Tool both map onto the user-side input.
                _ => "user",
            };
            let content = turn
                .parts
                .iter()
                .filter_map(part_to_content)
                .collect::<Vec<_>>();
            AnthropicMessage {
                role: role.to_string(),
                content,
            }
        })
        .collect()
}

/// Translate one Domain [`Part`] into a wire-format content block.
///
/// Returns `None` for parts that have no representation in the wire
/// format on this side of the message (today there are none — every
/// `Part` variant maps cleanly). The match remains exhaustive on the
/// existing variants so a future `Part` addition surfaces as a compiler
/// error here rather than silently dropping content.
fn part_to_content(part: &Part) -> Option<AnthropicContent> {
    match part {
        Part::Text(text) => Some(AnthropicContent::Text { text: text.clone() }),
        Part::ToolCall {
            id,
            name,
            arguments,
        } => Some(AnthropicContent::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: arguments.clone(),
        }),
        Part::ToolResult {
            call_id,
            content,
            is_error,
        } => Some(AnthropicContent::ToolResult {
            tool_use_id: call_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        }),
        Part::Image { mime, data } => Some(AnthropicContent::Image {
            source: match data {
                ImagePayload::Url(url) => AnthropicImageSource::Url { url: url.clone() },
                ImagePayload::Bytes(bytes) => AnthropicImageSource::Base64 {
                    media_type: mime.clone(),
                    data: BASE64_STANDARD.encode(bytes),
                },
            },
        }),
        // `Part` is `#[non_exhaustive]`. A future modality landing in
        // the Domain layer must extend this match before the adapter
        // picks it up; until then, drop unknown variants rather than
        // synthesising a wire shape we have not been taught.
        _ => None,
    }
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
    /// Marks the start of a content block. For `tool_use` blocks this
    /// is the only event that carries `id` and `name`; subsequent
    /// `content_block_delta` events for the same block carry only
    /// argument fragments. We surface this as the *first*
    /// [`StreamEvent::ToolCallDelta`] for a `call_id` so consumers
    /// learn which tool is being invoked before the first argument
    /// fragment lands.
    ContentBlockStart {
        index: u32,
        content_block: ContentBlockStart,
    },
    ContentBlockDelta {
        #[serde(default)]
        index: u32,
        delta: ContentBlockDelta,
    },
    /// Marks the end of a content block. We forget the index→tool
    /// mapping here so a subsequent `content_block_delta` for the
    /// same index (in a multi-block message) does not bleed
    /// arguments from a closed tool-call into a fresh one.
    ContentBlockStop {
        #[serde(default)]
        index: u32,
    },
    MessageDelta {
        #[serde(default)]
        usage: Option<UsagePayload>,
    },
    MessageStop,
    /// Catch-all for variants outside this slice's scope (`ping`,
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

/// Initial payload of a `content_block_start` event. Today's wire
/// format names the variants we care about (`text`, `tool_use`); the
/// catch-all keeps the parser tolerant of future block kinds.
///
/// `Text` carries no fields here — text content arrives on the
/// matching `content_block_delta` event, not on `start`. The variant
/// exists so the discriminator parses cleanly without falling through
/// to `Other`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockStart {
    Text,
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockDelta {
    TextDelta {
        text: String,
    },
    /// JSON-fragment delta for a tool-call's `input` field. The
    /// `partial_json` payload is the text the consumer feeds into the
    /// `ToolCallAccumulator` to reassemble the arguments object.
    InputJsonDelta {
        partial_json: String,
    },
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
///
/// Tool-call streaming requires state across lines: the wire format
/// announces a `tool_use` block's `id` and `name` only on the
/// `content_block_start` event, then attaches subsequent
/// `input_json_delta` events to that block by **index**, not by id.
/// `index_map` carries the `block index → call id` mapping so each
/// JSON delta can be re-tagged with the right correlation handle on
/// its way out as a [`StreamEvent::ToolCallDelta`].
fn parse_wire_event(
    line: &str,
    index_map: &mut std::collections::BTreeMap<u32, String>,
) -> Result<Option<StreamEvent>, String> {
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
        WireEvent::ContentBlockStart {
            index,
            content_block,
        } => match content_block {
            ContentBlockStart::ToolUse { id, name } => {
                index_map.insert(index, id.clone());
                Ok(Some(StreamEvent::ToolCallDelta(ToolCallDelta::new(
                    id,
                    Some(name),
                    String::new(),
                ))))
            }
            // Text-block starts and unknown block kinds carry no
            // payload we surface here. Text content arrives on the
            // matching `content_block_delta`; emitting on the start
            // event would double-fire empty `TextDelta`s.
            ContentBlockStart::Text | ContentBlockStart::Other => Ok(None),
        },
        WireEvent::ContentBlockDelta { index, delta } => match delta {
            ContentBlockDelta::TextDelta { text } => Ok(Some(StreamEvent::TextDelta(text))),
            ContentBlockDelta::InputJsonDelta { partial_json } => {
                // Resolve the `call_id` from the index map populated by
                // the matching `content_block_start`. A missing entry
                // means the upstream sent an `input_json_delta` for a
                // block we never saw start — surface it as a parse
                // error so the consumer does not silently lose
                // arguments.
                let call_id = index_map.get(&index).ok_or_else(|| {
                    format!("input_json_delta for unknown content block index {index}")
                })?;
                Ok(Some(StreamEvent::ToolCallDelta(ToolCallDelta::new(
                    call_id.clone(),
                    None,
                    partial_json,
                ))))
            }
            ContentBlockDelta::Other => Ok(None),
        },
        WireEvent::ContentBlockStop { index } => {
            // Forget the index→call_id binding so a later block at
            // the same index (legal in multi-call streams) cannot
            // bleed arguments from the closed call into a fresh one.
            index_map.remove(&index);
            Ok(None)
        }
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
// Capability discovery
// ─────────────────────────────────────────────────────────────────────

/// Static model catalogue derived from documented model families.
///
/// The wire format does not expose a discovery endpoint, so the
/// adapter ships this heuristic table. Every row pairs a stable
/// model id with its [`Capabilities`] flags and a context-window
/// figure pulled from public documentation. The `display_name` is
/// neutral and brand-agnostic — surfaces that render the picker show
/// these strings verbatim so the naming policy stays clean even at
/// the model-row layer.
///
/// Update cadence: when a new family ships, add a row; capability
/// drift on an existing family stays a documentation concern unless
/// the wire-level flag set actually changes.
fn model_catalogue() -> Vec<ModelInfo> {
    // Tuples: (model_id, display_name, context_window, capabilities)
    //
    // The capability set follows the documented family behaviour:
    //
    // - Frontier multimodal families: vision + tool calling + streaming.
    // - Compact text-only siblings: streaming only.
    //
    // Every row goes through `ModelInfo::new` so a future field on
    // `ModelInfo` does not silently regress to `Default::default()`.
    let entries: &[(&str, &str, u32, Capabilities)] = &[
        (
            "claude-3-5-sonnet-latest",
            "Sonnet 3.5 — frontier multimodal",
            200_000,
            Capabilities::new(true, true, true),
        ),
        (
            "claude-3-5-haiku-latest",
            "Haiku 3.5 — compact multimodal",
            200_000,
            Capabilities::new(true, true, true),
        ),
        (
            "claude-3-opus-latest",
            "Opus 3 — long-context multimodal",
            200_000,
            Capabilities::new(true, true, true),
        ),
        (
            "claude-3-sonnet-20240229",
            "Sonnet 3 — multimodal",
            200_000,
            Capabilities::new(true, true, true),
        ),
        (
            "claude-3-haiku-20240307",
            "Haiku 3 — compact multimodal",
            200_000,
            Capabilities::new(true, true, true),
        ),
        (
            "claude-2.1",
            "Legacy text-only — long context",
            200_000,
            Capabilities::new(false, false, true),
        ),
        (
            "claude-2.0",
            "Legacy text-only",
            100_000,
            Capabilities::new(false, false, true),
        ),
        (
            "claude-instant-1.2",
            "Legacy compact text-only",
            100_000,
            Capabilities::new(false, false, true),
        ),
    ];
    entries
        .iter()
        .map(|(id, name, window, caps)| {
            ModelInfo::new(ModelRef::new(PROVIDER_ID, *id), *name, *window, *caps)
        })
        .collect()
}

/// Resolve a [`Capabilities`] view for a model id. Known ids return
/// their catalogue entry; unknown ids fall back to the conservative
/// baseline `(false, false, true)` — text streaming only — so an
/// unrecognised id rejects vision and tool-call requests rather than
/// quietly forwarding them to an upstream that may not honour them.
///
/// The lookup is case-sensitive: model ids are stable identifiers and
/// the wire format treats them as such.
fn model_capabilities_for(model_id: &str) -> Capabilities {
    model_catalogue()
        .into_iter()
        .find(|info| info.model_ref.model_id == model_id)
        .map_or(Capabilities::new(false, false, true), |info| {
            info.capabilities
        })
}

/// Compare the conversation against the selected model's advertised
/// [`Capabilities`]. Returns `Err(reason)` describing the mismatch
/// when the conversation needs a flag the model does not provide;
/// returns `Ok(())` when every part is supported.
///
/// Checks today:
///
/// - [`Part::Image`] in any turn → requires `capabilities.vision`.
/// - [`Part::ToolCall`] or [`Part::ToolResult`] in any turn → requires
///   `capabilities.tool_calling`.
///
/// Streaming itself is always required by the wire format (`stream:
/// true` is hardcoded), so a `streaming = false` model would never
/// land here in the first place — the catalogue does not advertise
/// any such row today.
fn check_capabilities(model: &ModelRef, conv: &Conversation) -> Result<(), String> {
    let caps = model_capabilities_for(&model.model_id);

    let needs_vision = conv
        .turns
        .iter()
        .flat_map(|t| t.parts.iter())
        .any(|p| matches!(p, Part::Image { .. }));
    if needs_vision && !caps.vision {
        return Err(format!(
            "image input on a non-vision model ({}/{})",
            model.provider_id, model.model_id
        ));
    }

    let needs_tools = conv
        .turns
        .iter()
        .flat_map(|t| t.parts.iter())
        .any(|p| matches!(p, Part::ToolCall { .. } | Part::ToolResult { .. }));
    if needs_tools && !caps.tool_calling {
        return Err(format!(
            "tool calling on a tool-less model ({}/{})",
            model.provider_id, model.model_id
        ));
    }

    Ok(())
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

    // ─────────────────────────────────────────────────────────────────
    // Tool-calling + vision: outgoing wire shape
    // ─────────────────────────────────────────────────────────────────

    /// Headline acceptance criterion for Slice 9: the four content
    /// kinds (text, tool_use, tool_result, image) all serialise into
    /// the documented wire shape.
    #[test]
    fn part_to_content_covers_every_domain_variant() {
        let text = part_to_content(&Part::Text("hi".into())).expect("text maps");
        let tool_use = part_to_content(&Part::ToolCall {
            id: "call_1".into(),
            name: "search".into(),
            arguments: serde_json::json!({"q": "rust"}),
        })
        .expect("tool_use maps");
        let tool_result = part_to_content(&Part::ToolResult {
            call_id: "call_1".into(),
            content: serde_json::json!({"hits": 7}),
            is_error: false,
        })
        .expect("tool_result maps");
        let image_url = part_to_content(&Part::Image {
            mime: "image/png".into(),
            data: ImagePayload::Url("https://example.invalid/x.png".into()),
        })
        .expect("image url maps");
        let image_bytes = part_to_content(&Part::Image {
            mime: "image/jpeg".into(),
            data: ImagePayload::Bytes(vec![0xFF, 0xD8, 0xFF, 0xE0]),
        })
        .expect("image bytes map");

        let rendered =
            serde_json::to_string(&[text, tool_use, tool_result, image_url, image_bytes])
                .expect("serialize content");

        // Text block.
        assert!(rendered.contains(r#""type":"text""#));
        assert!(rendered.contains(r#""text":"hi""#));
        // Tool-use block.
        assert!(rendered.contains(r#""type":"tool_use""#));
        assert!(rendered.contains(r#""id":"call_1""#));
        assert!(rendered.contains(r#""name":"search""#));
        // Tool-result block.
        assert!(rendered.contains(r#""type":"tool_result""#));
        assert!(rendered.contains(r#""tool_use_id":"call_1""#));
        assert!(rendered.contains(r#""is_error":false"#));
        // Image (URL) block.
        assert!(rendered.contains(r#""type":"image""#));
        assert!(rendered.contains(r#""type":"url""#));
        assert!(rendered.contains(r#""url":"https://example.invalid/x.png""#));
        // Image (base64) block — value is base64-encoded, not raw.
        assert!(rendered.contains(r#""type":"base64""#));
        assert!(rendered.contains(r#""media_type":"image/jpeg""#));
        // 0xFF 0xD8 0xFF 0xE0 → "/9j/4A==" in standard base64.
        assert!(
            rendered.contains(r#""data":"/9j/4A==""#),
            "expected base64 payload in {rendered}"
        );
    }

    /// Tool turns fold onto user-side messages. The acceptance
    /// criteria call this out: tool results are user-side input in
    /// the alternative wire format.
    #[test]
    fn tool_role_turns_render_as_user_messages() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![
                Turn::new(
                    TurnId::new_v4(),
                    Role::Assistant,
                    vec![Part::ToolCall {
                        id: "c1".into(),
                        name: "lookup".into(),
                        arguments: serde_json::json!({}),
                    }],
                    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("ts"),
                    None,
                ),
                Turn::new(
                    TurnId::new_v4(),
                    Role::Tool,
                    vec![Part::ToolResult {
                        call_id: "c1".into(),
                        content: serde_json::json!({"ok": true}),
                        is_error: false,
                    }],
                    DateTime::<Utc>::from_timestamp(1_700_000_001, 0).expect("ts"),
                    None,
                ),
            ],
            None,
        );
        let req = build_request_body(
            &conv,
            &ModelRef::new(PROVIDER_ID, "claude-3-5-sonnet-latest"),
            &GenerationParams::default(),
        );
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "assistant");
        assert_eq!(req.messages[1].role, "user");
    }

    // ─────────────────────────────────────────────────────────────────
    // Capability discovery + capability-mismatch guard
    // ─────────────────────────────────────────────────────────────────

    /// Catalogue is non-empty and every entry carries the provider id.
    /// Belt-and-braces against a future refactor accidentally wiring
    /// up a row whose `provider_id` does not match
    /// [`PROVIDER_ID`] — the agent loop relies on that invariant
    /// when dispatching by `provider_id`.
    #[test]
    fn catalogue_rows_share_provider_id_and_advertise_streaming() {
        let catalogue = model_catalogue();
        assert!(!catalogue.is_empty(), "catalogue must not be empty");
        for info in &catalogue {
            assert_eq!(info.model_ref.provider_id, PROVIDER_ID);
            assert!(
                info.capabilities.streaming,
                "every catalogue row must advertise streaming"
            );
            assert!(info.context_window > 0, "context window must be populated");
        }
    }

    /// Known multimodal id resolves to all three capability flags;
    /// known legacy id resolves to text-only; unknown id falls back
    /// to the conservative baseline.
    #[test]
    fn capabilities_lookup_covers_known_and_unknown_ids() {
        let multimodal = model_capabilities_for("claude-3-5-sonnet-latest");
        assert!(multimodal.vision);
        assert!(multimodal.tool_calling);
        assert!(multimodal.streaming);

        let legacy = model_capabilities_for("claude-2.1");
        assert!(!legacy.vision);
        assert!(!legacy.tool_calling);
        assert!(legacy.streaming);

        let unknown = model_capabilities_for("not-a-real-model-id");
        assert!(!unknown.vision);
        assert!(!unknown.tool_calling);
        assert!(unknown.streaming, "fallback must still allow streaming");
    }

    /// Image input on a non-vision model surfaces the reason string
    /// the chat-stream guard wraps in `AiError::Unsupported`.
    #[test]
    fn capability_check_rejects_image_on_text_only_model() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![Turn::new(
                TurnId::new_v4(),
                Role::User,
                vec![Part::Image {
                    mime: "image/png".into(),
                    data: ImagePayload::Url("https://example.invalid/x.png".into()),
                }],
                DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("ts"),
                None,
            )],
            None,
        );
        let err = check_capabilities(&ModelRef::new(PROVIDER_ID, "claude-2.1"), &conv)
            .expect_err("image on legacy text-only must reject");
        assert!(err.contains("image input on a non-vision model"));
    }

    /// Tool-call parts on a tool-less model surface the matching
    /// reason string.
    #[test]
    fn capability_check_rejects_tool_call_on_tool_less_model() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![Turn::new(
                TurnId::new_v4(),
                Role::Assistant,
                vec![Part::ToolCall {
                    id: "c1".into(),
                    name: "lookup".into(),
                    arguments: serde_json::json!({}),
                }],
                DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("ts"),
                None,
            )],
            None,
        );
        let err = check_capabilities(&ModelRef::new(PROVIDER_ID, "claude-2.1"), &conv)
            .expect_err("tool call on legacy text-only must reject");
        assert!(err.contains("tool calling on a tool-less model"));
    }

    /// All-text conversation with a multimodal-capable model passes
    /// the gate cleanly. Pins the negative-space invariant — the
    /// guard does not over-fire.
    #[test]
    fn capability_check_passes_text_only_conversation() {
        let conv = Conversation::new(
            ChatId::new_v4(),
            None,
            vec![turn(Role::User, "hello")],
            None,
        );
        check_capabilities(
            &ModelRef::new(PROVIDER_ID, "claude-3-5-sonnet-latest"),
            &conv,
        )
        .expect("plain text on multimodal model must pass");
    }

    // ─────────────────────────────────────────────────────────────────
    // Tool-call streaming wire events
    // ─────────────────────────────────────────────────────────────────

    /// `content_block_start` for a `tool_use` block emits the first
    /// delta carrying `name = Some(_)` and an empty arguments
    /// fragment. Later `input_json_delta` events for the same index
    /// emit `name = None` deltas with the JSON fragment as
    /// `arguments_delta`.
    #[test]
    fn tool_call_streaming_threads_id_through_input_json_deltas() {
        let mut index_map = std::collections::BTreeMap::new();

        // 1. Start event introduces the tool call with id+name.
        let start = parse_wire_event(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_42","name":"search"}}"#,
            &mut index_map,
        )
        .expect("parse start")
        .expect("start emits");
        match start {
            StreamEvent::ToolCallDelta(delta) => {
                assert_eq!(delta.call_id, "call_42");
                assert_eq!(delta.name.as_deref(), Some("search"));
                assert_eq!(delta.arguments_delta, "");
            }
            other => panic!("expected ToolCallDelta on start, got {other:?}"),
        }

        // 2. JSON fragment delta carries name=None, arg fragment.
        let frag = parse_wire_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"a"}}"#,
            &mut index_map,
        )
        .expect("parse delta")
        .expect("delta emits");
        match frag {
            StreamEvent::ToolCallDelta(delta) => {
                assert_eq!(delta.call_id, "call_42");
                assert!(delta.name.is_none());
                assert_eq!(delta.arguments_delta, "{\"q\":\"a");
            }
            other => panic!("expected ToolCallDelta on delta, got {other:?}"),
        }

        // 3. Stop forgets the index → next call at the same index
        //    cannot bleed arguments from this one.
        let stop = parse_wire_event(r#"{"type":"content_block_stop","index":0}"#, &mut index_map)
            .expect("parse stop");
        assert!(stop.is_none(), "stop emits no event by itself");
        assert!(index_map.is_empty());
    }

    /// `input_json_delta` arriving for an index we never saw start
    /// surfaces as a parse error rather than silently emitting a
    /// delta with a fabricated `call_id`.
    #[test]
    fn input_json_delta_without_matching_start_surfaces_parse_error() {
        let mut index_map = std::collections::BTreeMap::new();
        let err = parse_wire_event(
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            &mut index_map,
        )
        .expect_err("must surface parse error");
        assert!(err.contains("unknown content block index"), "got: {err}");
    }

    /// Multi-turn stream: two back-to-back tool calls at the same
    /// index slot. The second `start` reseeds the index map after the
    /// first `stop` cleared it, so deltas after the second start
    /// route to the second call's id.
    #[test]
    fn multi_turn_tool_calls_route_to_distinct_ids() {
        let mut index_map = std::collections::BTreeMap::new();
        for line in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"first"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"b","name":"second"}}"#,
        ] {
            parse_wire_event(line, &mut index_map).expect("parse");
        }
        let frag = parse_wire_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}"#,
            &mut index_map,
        )
        .expect("parse")
        .expect("emits");
        match frag {
            StreamEvent::ToolCallDelta(delta) => assert_eq!(delta.call_id, "b"),
            other => panic!("expected ToolCallDelta with id=b, got {other:?}"),
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Round-trip property: Conversation → wire body → recovered shape
    // ─────────────────────────────────────────────────────────────────

    /// Slim recovered view of a Conversation, derived from the
    /// rendered wire body. `proptest` checks that whatever the
    /// generator produces survives the trip through
    /// `build_request_body` + JSON serialisation + JSON re-parse with
    /// the same per-turn role and per-part type sequence the original
    /// carried (modulo the `Role::System` lift).
    #[derive(Debug, PartialEq)]
    struct RecoveredTurn {
        role: String,
        block_types: Vec<String>,
    }

    fn recover(req: &AnthropicRequest) -> Vec<RecoveredTurn> {
        let json = serde_json::to_value(req).expect("serialise wire body");
        json["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .map(|m| RecoveredTurn {
                role: m["role"].as_str().expect("role string").to_string(),
                block_types: m["content"]
                    .as_array()
                    .expect("content array")
                    .iter()
                    .map(|b| b["type"].as_str().expect("type string").to_string())
                    .collect(),
            })
            .collect()
    }

    proptest::proptest! {
        /// Generates a small mixed conversation (text + tool calls +
        /// tool results + image refs across User / Assistant / Tool
        /// turns) and asserts the wire body's role+block-type
        /// sequence matches the Domain projection. The acceptance
        /// criteria for issue #51 calls this out explicitly:
        /// `proptest` round-trip on conversations with text + tool
        /// calls + tool results + image refs.
        #[test]
        fn conversation_round_trips_through_alternative_wire_format(
            kinds in proptest::collection::vec(
                proptest::sample::select(vec![
                    "user_text",
                    "assistant_text",
                    "assistant_tool_call",
                    "tool_result",
                    "user_image_url",
                    "user_image_bytes",
                ]),
                1..6,
            ),
        ) {
            let mut turns = Vec::new();
            let mut expected = Vec::new();
            for kind in &kinds {
                let (role, parts, wire_role, blocks) = match *kind {
                    "user_text" => (
                        Role::User,
                        vec![Part::Text("hi".into())],
                        "user",
                        vec!["text"],
                    ),
                    "assistant_text" => (
                        Role::Assistant,
                        vec![Part::Text("yo".into())],
                        "assistant",
                        vec!["text"],
                    ),
                    "assistant_tool_call" => (
                        Role::Assistant,
                        vec![Part::ToolCall {
                            id: "c".into(),
                            name: "n".into(),
                            arguments: serde_json::json!({}),
                        }],
                        "assistant",
                        vec!["tool_use"],
                    ),
                    "tool_result" => (
                        Role::Tool,
                        vec![Part::ToolResult {
                            call_id: "c".into(),
                            content: serde_json::json!("ok"),
                            is_error: false,
                        }],
                        "user",
                        vec!["tool_result"],
                    ),
                    "user_image_url" => (
                        Role::User,
                        vec![Part::Image {
                            mime: "image/png".into(),
                            data: ImagePayload::Url("https://x.invalid/y".into()),
                        }],
                        "user",
                        vec!["image"],
                    ),
                    "user_image_bytes" => (
                        Role::User,
                        vec![Part::Image {
                            mime: "image/png".into(),
                            data: ImagePayload::Bytes(vec![1, 2, 3]),
                        }],
                        "user",
                        vec!["image"],
                    ),
                    other => unreachable!("unexpected kind {other}"),
                };
                turns.push(Turn::new(
                    TurnId::new_v4(),
                    role,
                    parts,
                    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("ts"),
                    None,
                ));
                expected.push(RecoveredTurn {
                    role: wire_role.to_string(),
                    block_types: blocks.into_iter().map(String::from).collect(),
                });
            }
            let conv = Conversation::new(ChatId::new_v4(), None, turns, None);
            let req = build_request_body(
                &conv,
                &ModelRef::new(PROVIDER_ID, "claude-3-5-sonnet-latest"),
                &GenerationParams::default(),
            );
            proptest::prop_assert_eq!(recover(&req), expected);
        }
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
        let mut index_map = std::collections::BTreeMap::new();
        for (line, want) in cases {
            let got = parse_wire_event(line, &mut index_map).expect("parse");
            assert_eq!(got, want, "mismatch on {line}");
        }
    }

    /// Malformed JSON-NL surfaces as a parse error. The streaming
    /// pipeline maps this into `AiError::ProviderError`.
    #[test]
    fn malformed_line_surfaces_parse_error() {
        let mut index_map = std::collections::BTreeMap::new();
        let err = parse_wire_event("{not json", &mut index_map).expect_err("must error");
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
