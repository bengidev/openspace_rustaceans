//! Wire-format types for the industry-standard chat-completions API.
//!
//! These structs are private to the [`super`] module. They mirror the
//! request, streaming response, and `/models` discovery shapes the
//! upstream service speaks. The earlier text-only slice covered just
//! a string `content` field on each message; this slice extends the
//! envelope to:
//!
//! - **Structured `content`** — either a plain string (the text-only
//!   shape kept verbatim for backwards compatibility) or an array of
//!   typed parts carrying `text` and `image_url` blocks.
//! - **`tool_calls` on assistant messages** — the wire-format
//!   counterpart to [`Part::ToolCall`]. JSON arguments ride the wire
//!   as a serialised string per the upstream specification, not as
//!   structured JSON, even when the original Domain payload is a
//!   `serde_json::Value`.
//! - **`tool` role messages** — paired responses to a previous
//!   `tool_calls` entry. Carry `tool_call_id` matching the originating
//!   call plus a string `content` body.
//! - **`/models` response** — discovery endpoint surfaces the
//!   per-model `id`, optional `owned_by`, optional context window
//!   variants observed in the wild (`context_window` /
//!   `context_length` / `max_context_length`), and an optional
//!   `supported_parameters` list the catalogue layer parses for
//!   capability hints.
//!
//! Two design notes carried over from the text-only slice:
//!
//! - **`#[serde(skip_serializing_if = "...")]`** on every optional
//!   request field. The upstream API accepts any subset of the
//!   parameter set and treats omitted fields as defaults; emitting
//!   them as `null` produces a 400 from at least one well-known
//!   compatible implementation.
//! - **`#[serde(default)]`** on every optional response field. The
//!   streaming wire shape varies between implementations — some emit
//!   `usage` only on the final chunk, some never, some emit
//!   `finish_reason` as `null` mid-stream — and every field this
//!   parser touches has to tolerate absence.
//!
//! [`Part::ToolCall`]: openspace_shared::ai::domain::Part::ToolCall

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────
// Request envelope
// ─────────────────────────────────────────────────────────────────────

/// Request body for `POST /v1/chat/completions`.
///
/// Borrowed fields keep the body construction allocation-light where
/// the message contents are owned by the caller and live for the
/// duration of the request. `messages` and the per-message content
/// arrays own their data because the conversion from
/// [`openspace_shared::ai::domain::Conversation`] needs to allocate
/// fresh `String`s and structured parts.
#[derive(Debug, Serialize)]
pub(super) struct ChatRequest<'a> {
    pub(super) model: &'a str,
    pub(super) messages: Vec<ChatMessage>,
    pub(super) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) stop: Vec<&'a str>,
    /// Opt-in usage reporting on the final chunk. Some implementations
    /// of this wire format only attach a `usage` block when the
    /// caller asks for it via this option object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize)]
pub(super) struct StreamOptions {
    pub(super) include_usage: bool,
}

/// One message in the request `messages` array.
///
/// `content` is `Option` because an assistant message that emits only
/// `tool_calls` (no prose) carries `null` content per the upstream
/// spec. `tool_calls` is `None` for non-assistant messages and for
/// assistant messages that did not invoke a tool. `tool_call_id` is
/// `Some` only on `role = "tool"` messages where it correlates the
/// result back to the originating call.
#[derive(Debug, Serialize)]
pub(super) struct ChatMessage {
    pub(super) role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_calls: Option<Vec<ToolCallRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_call_id: Option<String>,
}

/// Either a plain string body (the text-only shape) or an array of
/// typed parts (the multimodal shape carrying images alongside text).
///
/// Untagged so the rendered JSON matches the upstream surface
/// verbatim — a string for simple messages, an array for messages
/// that interleave text and images.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum MessageContent {
    /// Plain prose. The earlier slice always emitted this shape; the
    /// extended slice keeps it for messages that contain a single
    /// text part so the wire shape does not regress.
    Text(String),
    /// Ordered list of typed content parts. Used whenever a message
    /// carries an image, multiple text parts, or a mix.
    Parts(Vec<ContentPart>),
}

/// One element of a [`MessageContent::Parts`] array.
///
/// Variants are tagged by `type` exactly as the upstream wire format
/// expects (`text`, `image_url`). Future modalities (audio, video)
/// land here without churning the surrounding shape.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlBlock },
}

/// Body of a `image_url` content part.
///
/// `url` carries either a remote `https://…` reference or an inline
/// `data:` URL — the same envelope the upstream accepts for both
/// remote and inline images. The Domain side translates
/// [`openspace_shared::ai::domain::ImagePayload::Bytes`] into a
/// `data:<mime>;base64,…` URL during the conversion.
#[derive(Debug, Serialize)]
pub(super) struct ImageUrlBlock {
    pub(super) url: String,
}

/// Outgoing tool-call request entry on an assistant message.
///
/// Wire format wraps every tool call in `{ id, type: "function",
/// function: { name, arguments } }`. The `arguments` field is a
/// serialised JSON string — not a structured value — because the
/// upstream contract pins it that way. Callers serialise the
/// Domain-side `serde_json::Value` to a string before constructing
/// this envelope.
#[derive(Debug, Serialize)]
pub(super) struct ToolCallRequest {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) call_type: &'static str,
    pub(super) function: ToolCallFunction,
}

#[derive(Debug, Serialize)]
pub(super) struct ToolCallFunction {
    pub(super) name: String,
    /// Serialised JSON string. The upstream wire format does **not**
    /// accept a structured value here.
    pub(super) arguments: String,
}

// ─────────────────────────────────────────────────────────────────────
// Streaming response envelope
// ─────────────────────────────────────────────────────────────────────

/// One streaming chunk delivered as the body of a `data:` SSE event.
///
/// `usage` is `Option` because most chunks carry text deltas and no
/// usage block; the final chunk on streams that opt in to usage
/// reporting carries the only populated `usage`.
#[derive(Debug, Deserialize)]
pub(super) struct ChatStreamChunk {
    #[serde(default)]
    pub(super) choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub(super) usage: Option<UsageWire>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ChunkChoice {
    #[serde(default)]
    pub(super) delta: ChunkDelta,
}

/// Delta payload on a streaming chunk.
///
/// Three independent channels can fire on a single chunk: text
/// content, tool-call fragments, and (rarely) a role marker — but
/// only the first two are surfaced upstream. `tool_calls` arrives as
/// a sparse array indexed by call slot: the first chunk for a slot
/// carries `id`, `type`, and `function.name`; subsequent chunks for
/// the same slot carry only the next `function.arguments` fragment.
/// The pump in [`super::provider`] correlates fragments across
/// chunks via the `index` field.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ChunkDelta {
    #[serde(default)]
    pub(super) content: Option<String>,
    #[serde(default)]
    pub(super) tool_calls: Vec<ToolCallChunk>,
}

/// Streaming tool-call fragment.
///
/// Every field is optional. The first chunk for a given `index`
/// typically carries `id` and `function.name`; later chunks carry
/// only `function.arguments`. Implementations also vary on whether
/// they re-send `id` on every chunk; the pump tolerates both.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ToolCallChunk {
    #[serde(default)]
    pub(super) index: u32,
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) function: Option<ToolCallChunkFunction>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ToolCallChunkFunction {
    #[serde(default)]
    pub(super) name: Option<String>,
    /// JSON-fragment for the running arguments buffer. Concatenated
    /// across chunks belonging to the same call slot.
    #[serde(default)]
    pub(super) arguments: Option<String>,
}

/// Token-accounting block on the final chunk.
///
/// Field naming mirrors the wire shape (`prompt_tokens`,
/// `completion_tokens`, `prompt_tokens_details`) rather than the
/// Domain shape (`input_tokens`, `output_tokens`,
/// `cached_input_tokens`); the conversion happens in [`super`].
#[derive(Debug, Deserialize)]
pub(super) struct UsageWire {
    #[serde(default)]
    pub(super) prompt_tokens: u32,
    #[serde(default)]
    pub(super) completion_tokens: u32,
    #[serde(default)]
    pub(super) prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PromptTokensDetails {
    #[serde(default)]
    pub(super) cached_tokens: u32,
}

// ─────────────────────────────────────────────────────────────────────
// /models response envelope
// ─────────────────────────────────────────────────────────────────────

/// Top-level shape of `GET /v1/models`.
///
/// `object` ("list") and the surrounding pagination fields are
/// ignored — we only consume `data`. Implementations diverge on
/// pagination behaviour; this slice consumes whatever the first
/// response carries and leaves multi-page support for a later
/// iteration.
#[derive(Debug, Deserialize)]
pub(super) struct ModelsResponse {
    #[serde(default)]
    pub(super) data: Vec<ModelEntry>,
}

/// One row of the `/models` `data` array.
///
/// Field names mirror the most-common shape across compatible
/// implementations. Missing fields are tolerated because:
///
/// - `owned_by` is informational; the catalogue keeps the model id
///   even when the upstream omits it.
/// - The three context-window aliases collapse onto one `Option<u32>`
///   in [`Self::context_window_value`] — implementations split
///   between `context_window`, `context_length`, and
///   `max_context_length` without a clear winner.
/// - `supported_parameters` is the capability hint the catalogue
///   parses dynamically; absence flips the catalogue into the
///   heuristic / fallback path.
#[derive(Debug, Deserialize)]
pub(super) struct ModelEntry {
    pub(super) id: String,
    #[serde(default)]
    pub(super) owned_by: Option<String>,
    #[serde(default)]
    pub(super) context_window: Option<u32>,
    #[serde(default)]
    pub(super) context_length: Option<u32>,
    #[serde(default)]
    pub(super) max_context_length: Option<u32>,
    /// Free-form list of capability tokens. Tokens we recognise:
    /// `tools` / `function_calling` → tool calling; `vision` /
    /// `image_url` / `images` / `multimodal` → vision; `stream` /
    /// `streaming` → streaming. Unknown tokens are ignored.
    #[serde(default)]
    pub(super) supported_parameters: Option<Vec<String>>,
}

impl ModelEntry {
    /// Collapse the three context-window aliases onto a single value,
    /// preferring the most specific name when more than one is
    /// populated.
    pub(super) fn context_window_value(&self) -> Option<u32> {
        self.context_window
            .or(self.context_length)
            .or(self.max_context_length)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Error envelope
// ─────────────────────────────────────────────────────────────────────

/// Error envelope returned on non-2xx responses.
///
/// The upstream wire format wraps every documented error in a
/// `{ "error": { "message": "...", "type": "...", "code": "..." } }`
/// envelope. We extract the message for the human-readable
/// diagnostic and discard the rest — the categorised mapping
/// happens in [`super::error_map`] based on the HTTP status code,
/// not the envelope contents.
#[derive(Debug, Deserialize)]
pub(super) struct ErrorEnvelope {
    pub(super) error: ErrorBody,
}

#[derive(Debug, Deserialize)]
pub(super) struct ErrorBody {
    pub(super) message: String,
}
