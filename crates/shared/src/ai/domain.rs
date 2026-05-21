//! Conversation Domain types — [`Conversation`], [`Turn`], [`Part`],
//! [`Role`], [`ModelRef`], [`ModelInfo`], [`Capabilities`],
//! [`GenerationParams`], [`Usage`].
//!
//! These shapes describe an in-progress chat from the agent loop's point
//! of view. The provider trait surface in [`super::provider`] consumes
//! them; the persistence layer (PRD-03) stores them; the UI layer
//! renders them. None of those concerns leak into this module —
//! everything here is pure data with serde round-trip and `Send + Sync`
//! guarantees pinned at compile time.
//!
//! # Public surface at a glance
//!
//! - [`Role`] — who produced a turn (`System`, `User`, `Assistant`, `Tool`).
//! - [`Part`] — the smallest piece of a turn. Text, tool call, tool
//!   result, or image. Marked `#[non_exhaustive]` so adding a future
//!   modality (audio, video) is non-breaking.
//! - [`ImagePayload`] — `Url(String) | Bytes(Vec<u8>)`, the two ways an
//!   image part carries pixels.
//! - [`Turn`] — one message in a conversation: id, role, parts, when it
//!   was created, and which model produced it (when known).
//! - [`Conversation`] — the whole thread: id, optional title, optional
//!   system prompt, and the ordered list of [`Turn`]s.
//! - [`ModelRef`] — `(provider_id, model_id)` handle that survives the
//!   persistence round-trip even when the runtime has not loaded the
//!   provider yet.
//! - [`Capabilities`] — three flags advertised by a provider/model
//!   pair: `vision`, `tool_calling`, `streaming`.
//! - [`ModelInfo`] — `ModelRef` + display name + context window +
//!   capabilities. The unit a model picker consumes.
//! - [`GenerationParams`] — sampling knobs (`temperature`, `top_p`,
//!   `max_tokens`, `stop`). Every field is optional / defaultable so a
//!   caller can pass `GenerationParams::default()` and let the provider
//!   pick.
//! - [`Usage`] — token-accounting record returned by providers
//!   alongside or after a stream finishes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

use crate::id::{ChatId, TurnId};

// ─────────────────────────────────────────────────────────────────────
// Role — who produced a turn. Closed set: every conversation surface in
// the product is one of these four. Adding a new role is a deliberate
// schema change so the enum stays exhaustive (no `#[non_exhaustive]`).
// ─────────────────────────────────────────────────────────────────────

/// Author of a [`Turn`].
///
/// `System` carries the system prompt or other framing instructions,
/// `User` the human's input, `Assistant` the model's reply, and `Tool`
/// the synthetic turn that wraps a tool result before it is fed back
/// into the next assistant pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Framing instructions ahead of the conversation proper.
    System,
    /// Human author.
    User,
    /// Model author.
    Assistant,
    /// Synthetic role wrapping a tool result for the next assistant pass.
    Tool,
}

assert_impl_all!(Role: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ImagePayload + Part — the smallest unit a turn carries. `Part` is
// `#[non_exhaustive]` so future modalities (audio, video, structured
// blocks) can land without a breaking change. Consumers must use a
// wildcard arm; the round-trip test below pins that contract.
// ─────────────────────────────────────────────────────────────────────

/// Pixel payload for an [`Part::Image`].
///
/// `Url` defers fetching to the provider (cheap to ship across an
/// adapter boundary, but the provider must be able to reach the URL).
/// `Bytes` carries the encoded image inline — costlier on the wire but
/// works for local files and offline workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagePayload {
    /// Remote reference. The provider fetches the URL itself.
    Url(String),
    /// Inline encoded bytes. The mime type lives on the parent
    /// [`Part::Image`] field.
    Bytes(Vec<u8>),
}

assert_impl_all!(ImagePayload: Send, Sync);

/// One piece of a [`Turn`].
///
/// A turn is an ordered list of parts so a single assistant reply can
/// interleave prose, tool calls, tool results, and images. Marked
/// `#[non_exhaustive]` so adding new modalities is non-breaking.
///
/// Tool-related variants split into two:
///
/// - [`Part::ToolCall`] — the assistant requesting a tool invocation.
///   The `id` field is what the matching [`Part::ToolResult`] echoes
///   back as `call_id` so the agent loop can correlate them.
/// - [`Part::ToolResult`] — the tool's response, content carried as
///   `serde_json::Value` so the runtime can pass through arbitrary
///   provider-specific shapes without bespoke wrappers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Part {
    /// Plain text fragment.
    Text(String),

    /// Assistant-issued tool invocation request.
    ToolCall {
        /// Correlation handle matched by [`Part::ToolResult::call_id`].
        id: String,
        /// Tool name as advertised by the tool registry.
        name: String,
        /// JSON arguments. Schema is the tool's responsibility.
        arguments: serde_json::Value,
    },

    /// Tool runtime's response to a [`Part::ToolCall`].
    ToolResult {
        /// Echoes the matching [`Part::ToolCall::id`].
        call_id: String,
        /// Result payload. Free-form JSON so providers and tools can
        /// agree on schema without churn here.
        content: serde_json::Value,
        /// `true` when the tool returned an error result. Surfaces the
        /// failure shape distinctly from the success one even when
        /// both happen to be JSON.
        is_error: bool,
    },

    /// Image part. `mime` describes the encoding (`image/png`,
    /// `image/jpeg`, …) and `data` carries the pixels themselves via
    /// [`ImagePayload`].
    Image {
        /// IANA-style mime type.
        mime: String,
        /// Pixel payload.
        data: ImagePayload,
    },
}

assert_impl_all!(Part: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ModelRef + Capabilities + ModelInfo — the model identity + capability
// triple. ModelRef is the lightweight handle that survives the
// persistence round-trip; ModelInfo wraps it with the surface a UI
// picker needs to render.
// ─────────────────────────────────────────────────────────────────────

/// Stable handle to a model across the persistence layer.
///
/// `provider_id` matches [`super::provider::AiProvider::id`]; `model_id`
/// is the provider-specific identifier the adapter accepts. The pair
/// round-trips through serde unchanged so a turn whose `model` field
/// references a not-yet-loaded provider stays correlatable after a
/// restart.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelRef {
    /// Provider identifier as advertised by the adapter.
    pub provider_id: String,
    /// Provider-specific model identifier.
    pub model_id: String,
}

impl ModelRef {
    /// Bundle a fresh handle. Wrapping struct construction keeps
    /// `#[non_exhaustive]` meaningful — adding a new optional field
    /// later (e.g. `region`) stays non-breaking for callers that go
    /// through this constructor.
    #[must_use]
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        }
    }
}

assert_impl_all!(ModelRef: Send, Sync);

/// Three independent capability flags a `(provider, model)` pair
/// advertises.
///
/// `vision` covers image-in inputs, `tool_calling` covers structured
/// tool-call output, `streaming` covers token-by-token delivery via the
/// provider's streaming API. Implementations consult these flags to
/// decide whether to take a feature path; the UI consults them to
/// disable controls that would not work.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Capabilities {
    /// Model accepts image parts in its input.
    pub vision: bool,
    /// Model can emit structured tool calls.
    pub tool_calling: bool,
    /// Model supports server-streamed deltas.
    pub streaming: bool,
}

impl Capabilities {
    /// Construct a capability set from explicit flags.
    #[must_use]
    pub const fn new(vision: bool, tool_calling: bool, streaming: bool) -> Self {
        Self {
            vision,
            tool_calling,
            streaming,
        }
    }
}

assert_impl_all!(Capabilities: Send, Sync);

/// Renderable model description.
///
/// The model picker consumes this — `display_name` for the row label,
/// `context_window` for the size badge, `capabilities` for the
/// inline-feature icons, `model_ref` for selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelInfo {
    /// Stable handle that survives a restart.
    pub model_ref: ModelRef,
    /// Human-friendly label rendered in the picker.
    pub display_name: String,
    /// Maximum context window in tokens.
    pub context_window: u32,
    /// Capability flags consulted by feature gates.
    pub capabilities: Capabilities,
}

impl ModelInfo {
    /// Bundle the four fields. Same `#[non_exhaustive]` rationale as
    /// [`ModelRef::new`].
    #[must_use]
    pub fn new(
        model_ref: ModelRef,
        display_name: impl Into<String>,
        context_window: u32,
        capabilities: Capabilities,
    ) -> Self {
        Self {
            model_ref,
            display_name: display_name.into(),
            context_window,
            capabilities,
        }
    }
}

assert_impl_all!(ModelInfo: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// GenerationParams + Usage — sampling knobs and token accounting.
// ─────────────────────────────────────────────────────────────────────

/// Sampling parameters passed to a provider call.
///
/// Every field is optional or has a sensible zero so
/// `GenerationParams::default()` is always meaningful — providers fall
/// back to their own defaults when a knob is `None`. `stop` defaults to
/// an empty `Vec<String>` rather than `Option<Vec<_>>` because the
/// "no stop strings" semantics map cleanly to the empty list and avoid
/// a nested `Option` on the hot path.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GenerationParams {
    /// Sampling temperature. `None` defers to the provider default.
    pub temperature: Option<f32>,
    /// Nucleus sampling threshold. `None` defers to the provider
    /// default.
    pub top_p: Option<f32>,
    /// Hard cap on generated tokens. `None` defers to the provider
    /// default.
    pub max_tokens: Option<u32>,
    /// Stop strings. Empty when no stops are configured; non-empty
    /// when the caller wants generation to halt at any of these
    /// substrings.
    pub stop: Vec<String>,
}

impl GenerationParams {
    /// Construct from explicit values. Pairs with `#[non_exhaustive]`
    /// the same way [`ModelRef::new`] does.
    #[must_use]
    pub fn new(
        temperature: Option<f32>,
        top_p: Option<f32>,
        max_tokens: Option<u32>,
        stop: Vec<String>,
    ) -> Self {
        Self {
            temperature,
            top_p,
            max_tokens,
            stop,
        }
    }
}

// `Eq` is intentionally not derived: `f32` does not implement it.
// `PartialEq` covers the round-trip test contract; downstream code that
// needs hashing or strict equality should compare structurally on the
// fields it cares about.
assert_impl_all!(GenerationParams: Send, Sync);

/// Token-accounting record returned by providers.
///
/// `cached_input_tokens` is reported separately from `input_tokens`
/// because providers that support prompt caching bill the cached and
/// uncached portions at different rates — keeping them split lets the
/// telemetry layer compute cost without re-deriving the breakdown.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// Tokens consumed from the prompt (uncached portion).
    pub input_tokens: u32,
    /// Tokens emitted in the response.
    pub output_tokens: u32,
    /// Tokens served from the provider's prompt cache.
    pub cached_input_tokens: u32,
}

impl Usage {
    /// Construct an explicit usage record.
    #[must_use]
    pub const fn new(input_tokens: u32, output_tokens: u32, cached_input_tokens: u32) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        }
    }
}

assert_impl_all!(Usage: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Turn + Conversation — the headline aggregates. Both are
// `#[non_exhaustive]` so PRD-03 (persistence) can grow the records
// without a breaking change for downstream construction sites.
// ─────────────────────────────────────────────────────────────────────

/// One message in a conversation.
///
/// `parts` is ordered — a single assistant turn can interleave text,
/// tool calls, and images in the order the model produced them. `model`
/// is `Option<ModelRef>` because user and tool turns do not carry a
/// model attribution; assistant turns always do once the agent loop
/// resolves the active model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Turn {
    /// Stable identity. Comes from [`crate::id::TurnId`].
    pub id: TurnId,
    /// Author of the turn.
    pub role: Role,
    /// Ordered parts that make up the turn body.
    pub parts: Vec<Part>,
    /// Wall-clock timestamp at turn creation. UTC so storage and UI
    /// can reformat for the local time zone without ambiguity.
    pub created_at: DateTime<Utc>,
    /// Model that produced the turn, when known. `None` for user and
    /// tool turns.
    pub model: Option<ModelRef>,
}

impl Turn {
    /// Bundle a fully populated turn. Same `#[non_exhaustive]`
    /// rationale as the other Domain constructors.
    #[must_use]
    pub fn new(
        id: TurnId,
        role: Role,
        parts: Vec<Part>,
        created_at: DateTime<Utc>,
        model: Option<ModelRef>,
    ) -> Self {
        Self {
            id,
            role,
            parts,
            created_at,
            model,
        }
    }
}

assert_impl_all!(Turn: Send, Sync);

/// The whole conversation thread.
///
/// `system_prompt` is carried separately from `turns` because providers
/// surface it as a distinct field — a per-conversation system prompt is
/// not the same as a `Role::System` turn appearing inline. Either one
/// or the other (or both) may be present; the agent loop is the layer
/// that decides how to render each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Conversation {
    /// Stable identity. Comes from [`crate::id::ChatId`].
    pub id: ChatId,
    /// Optional human-friendly title. `None` until the first auto- or
    /// user-rename.
    pub title: Option<String>,
    /// Ordered list of turns. Always at least one turn after the
    /// first user message lands.
    pub turns: Vec<Turn>,
    /// Optional per-conversation system prompt distinct from any
    /// `Role::System` turn. `None` when the workspace default applies.
    pub system_prompt: Option<String>,
}

impl Conversation {
    /// Bundle a fully populated conversation.
    #[must_use]
    pub fn new(
        id: ChatId,
        title: Option<String>,
        turns: Vec<Turn>,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
            id,
            title,
            turns,
            system_prompt,
        }
    }
}

assert_impl_all!(Conversation: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `Turn` whose `parts` field exercises every `Part`
    /// variant exactly once. The serde round-trip below uses this as
    /// the input fixture — if a future variant is added without a
    /// corresponding entry here, the test stays green silently and we
    /// lose coverage. Worth the manual maintenance because the variant
    /// surface is small.
    fn turn_with_every_part_variant() -> Turn {
        Turn::new(
            TurnId::new_v4(),
            Role::Assistant,
            vec![
                Part::Text("hello".to_string()),
                Part::ToolCall {
                    id: "call-1".to_string(),
                    name: "search".to_string(),
                    arguments: serde_json::json!({ "query": "rust async" }),
                },
                Part::ToolResult {
                    call_id: "call-1".to_string(),
                    content: serde_json::json!({ "hits": 7 }),
                    is_error: false,
                },
                Part::Image {
                    mime: "image/png".to_string(),
                    data: ImagePayload::Bytes(vec![0x89, 0x50, 0x4E, 0x47]),
                },
            ],
            DateTime::<Utc>::from_timestamp(1_700_000_000, 0)
                .expect("timestamp is in chrono's representable range"),
            Some(ModelRef::new("local", "model-x")),
        )
    }

    /// Headline acceptance criterion for issue #31: a `Conversation`
    /// containing a `Turn` with every `Part` variant round-trips
    /// through JSON unchanged.
    #[test]
    fn conversation_round_trips_through_json() {
        let original = Conversation::new(
            ChatId::new_v4(),
            Some("Round-trip fixture".to_string()),
            vec![turn_with_every_part_variant()],
            Some("be helpful".to_string()),
        );
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Conversation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// Round-trips a `ModelInfo` so the model-picker shape is locked.
    #[test]
    fn model_info_round_trips_through_json() {
        let original = ModelInfo::new(
            ModelRef::new("local", "model-x"),
            "Local Model X",
            128_000,
            Capabilities::new(true, true, true),
        );
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: ModelInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// Round-trips an explicit `GenerationParams`. `f32` lacks `Eq` so
    /// the assertion uses `PartialEq`, which is what the derive
    /// produces and what callers will use too.
    #[test]
    fn generation_params_round_trip_through_json() {
        let original = GenerationParams::new(
            Some(0.7),
            Some(0.95),
            Some(2048),
            vec!["</done>".to_string(), "STOP".to_string()],
        );
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: GenerationParams = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// Round-trips a `Usage` with non-zero values across every field.
    #[test]
    fn usage_round_trips_through_json() {
        let original = Usage::new(1_024, 256, 64);
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Usage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// `Part` is `#[non_exhaustive]`. The acceptance criteria require
    /// consumers to use a wildcard arm; this test pins that contract
    /// by exhaustively matching every variant followed by `_`. If a
    /// future variant lands without `#[non_exhaustive]` being honoured
    /// the wildcard arm becomes unreachable and clippy flags it.
    #[test]
    fn part_match_uses_wildcard_arm() {
        let parts = [
            Part::Text("a".to_string()),
            Part::ToolCall {
                id: "1".to_string(),
                name: "n".to_string(),
                arguments: serde_json::Value::Null,
            },
            Part::ToolResult {
                call_id: "1".to_string(),
                content: serde_json::Value::Null,
                is_error: false,
            },
            Part::Image {
                mime: "image/png".to_string(),
                data: ImagePayload::Url("file:///x".to_string()),
            },
        ];
        for p in parts {
            // Wildcard arm proves the variant set is treated as open.
            // The `unreachable_patterns` allow is exactly the contract
            // we want to pin: today every variant is named, so the `_`
            // arm is unreachable; if a future variant lands without
            // honouring `#[non_exhaustive]`, this allow stops being
            // necessary and the test still compiles cleanly.
            #[allow(unreachable_patterns)]
            let label = match p {
                Part::Text(_) => "text",
                Part::ToolCall { .. } => "tool_call",
                Part::ToolResult { .. } => "tool_result",
                Part::Image { .. } => "image",
                _ => "unknown",
            };
            assert!(matches!(
                label,
                "text" | "tool_call" | "tool_result" | "image"
            ));
        }
    }

    /// Locks the wire field names for `Conversation`. Persistence
    /// (PRD-03) keys on these names so a Rust rename refactor cannot
    /// silently break stored snapshots.
    #[test]
    fn conversation_wire_field_names_are_stable() {
        let value =
            serde_json::to_value(Conversation::new(ChatId::new_v4(), None, Vec::new(), None))
                .expect("serialize as value");
        let object = value.as_object().expect("object");
        for key in ["id", "title", "turns", "system_prompt"] {
            assert!(object.contains_key(key), "missing key {key}");
        }
        assert_eq!(object.len(), 4, "no unexpected fields");
    }
}
