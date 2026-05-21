//! Wire-format types for the industry-standard chat-completions API.
//!
//! These structs are private to the [`super`] module. They mirror the
//! request and streaming response shapes the upstream service speaks
//! and intentionally elide every field this slice does not need —
//! tool calling, function calling, vision parts, log-probabilities,
//! and the long tail of vendor-specific knobs land in the follow-up
//! slice that turns the bare text channel into the full surface.
//!
//! Two design notes:
//!
//! - **`#[serde(skip_serializing_if = "...")]`** on every optional
//!   request field. The upstream API accepts any subset of the
//!   parameter set and treats omitted fields as defaults; emitting
//!   them as `null` would produce a 400 from at least one
//!   well-known compatible implementation.
//! - **`#[serde(default)]`** on every optional response field. The
//!   streaming wire shape varies between implementations — some emit
//!   `usage` only on the final chunk, some never, some emit
//!   `finish_reason` as `null` mid-stream — and every field this
//!   parser touches has to tolerate absence.

use serde::{Deserialize, Serialize};

/// Request body for `POST /v1/chat/completions`.
///
/// Borrowed (`&str`) fields keep the body construction allocation-light
/// — the message contents are owned by the [`super::super`] caller and
/// live for the duration of the request.
#[derive(Debug, Serialize)]
pub(super) struct ChatRequest<'a> {
    pub(super) model: &'a str,
    pub(super) messages: Vec<ChatMessage<'a>>,
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
/// Roles are emitted as the lowercase strings the wire format
/// expects (`system`, `user`, `assistant`). Tool roles are not
/// emitted by this slice — the conversion in [`super`] rejects
/// non-text parts before reaching this struct.
#[derive(Debug, Serialize)]
pub(super) struct ChatMessage<'a> {
    pub(super) role: &'a str,
    pub(super) content: &'a str,
}

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

#[derive(Debug, Default, Deserialize)]
pub(super) struct ChunkDelta {
    #[serde(default)]
    pub(super) content: Option<String>,
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

/// Error envelope returned on non-2xx responses.
///
/// The upstream wire format wraps every documented error in a
/// `{ "error": { "message": "...", "type": "..." } }` envelope. We
/// extract the message for the human-readable diagnostic and
/// discard the rest — the categorised mapping happens in
/// [`super::error_map`] based on the HTTP status code, not the
/// envelope contents.
#[derive(Debug, Deserialize)]
pub(super) struct ErrorEnvelope {
    pub(super) error: ErrorBody,
}

#[derive(Debug, Deserialize)]
pub(super) struct ErrorBody {
    pub(super) message: String,
}
