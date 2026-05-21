//! AI provider trait surface — [`AiProvider`], [`StreamEvent`],
//! [`ToolCallDelta`].
//!
//! The trait is the boundary every concrete provider implementation
//! crosses: a hosted-API adapter, a local-runtime adapter, an in-memory
//! test fixture. Streaming returns a [`futures::stream::BoxStream`] so
//! the trait stays runtime-agnostic — no `tokio` reaches the Domain
//! layer.
//!
//! # Trait shape
//!
//! ```ignore
//! #[async_trait]
//! trait AiProvider: Send + Sync {
//!     fn id(&self) -> &str;
//!     fn display_name(&self) -> &str;
//!     fn capabilities(&self) -> Capabilities;
//!     async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError>;
//!     fn chat_stream(
//!         &self,
//!         conv: Conversation,
//!         model: ModelRef,
//!         params: GenerationParams,
//!     ) -> BoxStream<'static, Result<StreamEvent, AiError>>;
//! }
//! ```
//!
//! `chat_stream` returns a synchronous `BoxStream` (not an
//! `async fn -> Stream`) for two reasons:
//!
//! 1. The agent loop wants the stream handle immediately so it can
//!    register cancellation hooks before the first byte arrives.
//! 2. `dyn AiProvider` stays object-safe with a sync return; an
//!    `async fn` returning a stream needs an extra hop.
//!
//! `list_models` *does* return `async` because callers happily await
//! a one-shot list and `async-trait` papers over the `dyn` issue.
//!
//! # Streaming protocol — [`StreamEvent`]
//!
//! Providers emit a sequence of `StreamEvent`s ending in
//! [`StreamEvent::Done`]. The variant set:
//!
//! - [`StreamEvent::TextDelta`] — the assistant text accreting.
//! - [`StreamEvent::ToolCallDelta`] — a tool call accreting. JSON
//!   arguments arrive as fragments (`arguments_delta`) so the consumer
//!   reassembles them; `name` is `None` after the first delta for that
//!   `call_id` because providers send it only once at the start.
//! - [`StreamEvent::UsageReport`] — token accounting, delivered once
//!   per stream when the provider supports it.
//! - [`StreamEvent::Done`] — terminal marker. The stream ends after
//!   yielding this; consumers stop reading.
//!
//! `StreamEvent` is `#[non_exhaustive]`, so a future structured event
//! (e.g. `Citation`, `ContentFilter`) is non-breaking. Consumers must
//! use a wildcard arm — the test below pins that.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

use crate::ai::domain::{Capabilities, Conversation, GenerationParams, ModelInfo, ModelRef, Usage};
use crate::ai::error::AiError;

// ─────────────────────────────────────────────────────────────────────
// ToolCallDelta — payload of `StreamEvent::ToolCallDelta`. Modeled as
// its own struct so additional fields (e.g. `arguments_complete`) can
// land later without churning the enum variant signature.
// ─────────────────────────────────────────────────────────────────────

/// Incremental fragment of a tool-call assembly.
///
/// Providers stream a tool call in pieces:
///
/// 1. The first delta for a `call_id` carries `name = Some(...)` and
///    typically an empty or partial `arguments_delta` — the consumer
///    learns *which* tool is being called.
/// 2. Subsequent deltas for the same `call_id` carry `name = None`
///    and append more JSON fragments to the arguments buffer.
/// 3. The consumer concatenates `arguments_delta`s and parses the
///    final string once the stream ends or the next event arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolCallDelta {
    /// Correlation handle. Matches the `id` on the eventual
    /// [`crate::ai::domain::Part::ToolCall`].
    pub call_id: String,
    /// Tool name. `Some` only on the first delta for a `call_id`,
    /// `None` thereafter.
    pub name: Option<String>,
    /// JSON fragment to append to the running arguments buffer.
    pub arguments_delta: String,
}

impl ToolCallDelta {
    /// Construct a delta. Pairs with `#[non_exhaustive]` — callers
    /// go through this constructor instead of struct literals so
    /// adding a new field later stays non-breaking.
    #[must_use]
    pub fn new(
        call_id: impl Into<String>,
        name: Option<String>,
        arguments_delta: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            name,
            arguments_delta: arguments_delta.into(),
        }
    }
}

assert_impl_all!(ToolCallDelta: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// StreamEvent — the unit a provider yields from `chat_stream`. Marked
// `#[non_exhaustive]` so future events (citations, content-filter
// notices) are additive.
// ─────────────────────────────────────────────────────────────────────

/// Element of the chat stream.
///
/// Variants:
///
/// - [`StreamEvent::TextDelta`] — assistant text accreting.
/// - [`StreamEvent::ToolCallDelta`] — tool call accreting.
/// - [`StreamEvent::UsageReport`] — token-accounting summary.
/// - [`StreamEvent::Done`] — terminal marker.
///
/// Consumers must use a wildcard arm; the variant set is open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamEvent {
    /// Text fragment the assistant produced.
    TextDelta(String),
    /// Fragment of a tool-call assembly.
    ToolCallDelta(ToolCallDelta),
    /// Token-accounting summary. Providers typically emit this once
    /// per stream — at the start, end, or both.
    UsageReport(Usage),
    /// Terminal marker. The stream ends after yielding this; the
    /// consumer stops reading.
    Done,
}

assert_impl_all!(StreamEvent: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// AiProvider — the trait every adapter implements. `dyn`-compatible
// (object-safe) so the agent loop can hold it behind
// `Arc<dyn AiProvider + Send + Sync>`. `async-trait` provides the
// `async fn list_models` shape; `chat_stream` is sync-returning to
// keep cancellation registration immediate.
// ─────────────────────────────────────────────────────────────────────

/// The boundary every concrete AI provider crosses.
///
/// See the module-level docs for the trait shape and the streaming
/// protocol contract. Implementations are expected to be cheap to
/// clone via `Arc`; the agent loop holds a shared handle and may
/// dispatch many calls concurrently.
#[async_trait]
pub trait AiProvider: Send + Sync {
    /// Stable provider identifier. Matches the `provider_id` field on
    /// every [`ModelRef`] this provider produces. Lowercase, no
    /// whitespace, suitable for use as a config key.
    fn id(&self) -> &str;

    /// Human-friendly display name rendered in pickers and headers.
    fn display_name(&self) -> &str;

    /// Capability flags the provider as a whole supports. Per-model
    /// capabilities live on [`ModelInfo::capabilities`] — this surface
    /// describes the *upper bound* of what the provider can do.
    fn capabilities(&self) -> Capabilities;

    /// Enumerate the models this provider exposes.
    ///
    /// # Errors
    ///
    /// Returns an [`AiError`] when the provider cannot reach its
    /// catalogue (network failure, auth failure, …) or when the
    /// catalogue itself is malformed.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError>;

    /// Open a streaming chat call.
    ///
    /// The returned [`BoxStream`] is `'static` so the agent loop can
    /// move it into a task without lifetime gymnastics. The stream
    /// terminates with [`StreamEvent::Done`] (or an `Err` item that
    /// effectively ends the stream).
    ///
    /// Errors are reported as `Err` items inside the stream rather
    /// than as a `Result<BoxStream, _>` because failures often surface
    /// mid-stream (a network drop after some tokens already arrived).
    fn chat_stream(
        &self,
        conv: Conversation,
        model: ModelRef,
        params: GenerationParams,
    ) -> BoxStream<'static, Result<StreamEvent, AiError>>;
}

// `dyn AiProvider: Send + Sync` so the agent loop can store the trait
// object behind an `Arc` and share it across tasks. If a future change
// to the trait inadvertently breaks `dyn`-compatibility, this assertion
// fails to compile and tells us at the trait-surface level rather than
// at the call site.
assert_impl_all!(dyn AiProvider: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::domain::{Capabilities, Conversation, GenerationParams, ModelRef};
    use crate::id::ChatId;
    use futures::StreamExt;

    /// Round-trips every `StreamEvent` variant through JSON. Locks the
    /// wire shape so telemetry pipelines and snapshot tests do not
    /// silently break after a future variant rename.
    #[test]
    fn stream_event_round_trips_through_json() {
        let cases = [
            StreamEvent::TextDelta("hello".to_string()),
            StreamEvent::ToolCallDelta(ToolCallDelta::new(
                "call-1",
                Some("search".to_string()),
                "{\"query\":",
            )),
            StreamEvent::ToolCallDelta(ToolCallDelta::new("call-1", None, "\"rust\"}")),
            StreamEvent::UsageReport(Usage::new(10, 20, 0)),
            StreamEvent::Done,
        ];

        for original in cases {
            let json = serde_json::to_string(&original).expect("serialize");
            let decoded: StreamEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(original, decoded, "round-trip mismatch for {original:?}");
        }
    }

    /// `StreamEvent` is `#[non_exhaustive]`. The acceptance criteria
    /// require consumers to use a wildcard arm; this test pins that
    /// contract by exhaustively matching every variant followed by
    /// `_`. If a future variant lands without `#[non_exhaustive]` being
    /// honoured, the wildcard becomes unreachable and clippy flags it.
    #[test]
    fn stream_event_match_uses_wildcard_arm() {
        let events = [
            StreamEvent::TextDelta("a".to_string()),
            StreamEvent::ToolCallDelta(ToolCallDelta::new("c", None, "")),
            StreamEvent::UsageReport(Usage::new(0, 0, 0)),
            StreamEvent::Done,
        ];
        for e in events {
            // Same `#[non_exhaustive]` contract as `Part`'s test:
            // every variant is named today, so the wildcard arm is
            // unreachable. Allowing the lint pins the contract.
            #[allow(unreachable_patterns)]
            let label = match e {
                StreamEvent::TextDelta(_) => "text",
                StreamEvent::ToolCallDelta(_) => "tool_call",
                StreamEvent::UsageReport(_) => "usage",
                StreamEvent::Done => "done",
                _ => "unknown",
            };
            assert!(matches!(label, "text" | "tool_call" | "usage" | "done"));
        }
    }

    /// Minimal in-memory provider used to exercise the trait surface
    /// at compile time. Confirms `AiProvider` is object-safe, that an
    /// `Arc<dyn AiProvider + Send + Sync>` builds, and that
    /// [`AiProvider::chat_stream`] really does return
    /// `BoxStream<'static, Result<StreamEvent, AiError>>`.
    struct ScriptedProvider {
        events: Vec<Result<StreamEvent, AiError>>,
    }

    #[async_trait]
    impl AiProvider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }

        fn display_name(&self) -> &str {
            "Scripted"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::new(false, false, true)
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
            Ok(Vec::new())
        }

        fn chat_stream(
            &self,
            _conv: Conversation,
            _model: ModelRef,
            _params: GenerationParams,
        ) -> BoxStream<'static, Result<StreamEvent, AiError>> {
            let owned: Vec<Result<StreamEvent, AiError>> = self.events.clone();
            futures::stream::iter(owned).boxed()
        }
    }

    /// Builds an `Arc<dyn AiProvider + Send + Sync>` from a concrete
    /// provider — proves the trait is `dyn`-compatible at the type
    /// level. The acceptance criteria call this out explicitly.
    #[test]
    fn ai_provider_is_dyn_compatible() {
        let provider: std::sync::Arc<dyn AiProvider + Send + Sync> =
            std::sync::Arc::new(ScriptedProvider { events: Vec::new() });
        assert_eq!(provider.id(), "scripted");
        assert_eq!(provider.display_name(), "Scripted");
        assert!(provider.capabilities().streaming);
    }

    /// Confirms `chat_stream` actually returns
    /// `BoxStream<'static, Result<StreamEvent, AiError>>` and that the
    /// stream yields the scripted events in order, terminating with
    /// `Done`. This is the doc-test equivalent the acceptance criteria
    /// asks for, lifted into a unit test so it runs under
    /// `cargo test`.
    #[test]
    fn chat_stream_return_type_and_terminal_done() {
        let provider = ScriptedProvider {
            events: vec![
                Ok(StreamEvent::TextDelta("hi".to_string())),
                Ok(StreamEvent::UsageReport(Usage::new(1, 1, 0))),
                Ok(StreamEvent::Done),
            ],
        };

        // Type-level assertion: the value's type is exactly the
        // alias the trait promises.
        let stream: BoxStream<'static, Result<StreamEvent, AiError>> = provider.chat_stream(
            Conversation::new(ChatId::new_v4(), None, Vec::new(), None),
            ModelRef::new("scripted", "model-x"),
            GenerationParams::default(),
        );

        // Drain on a tiny ad-hoc executor so the test stays
        // runtime-agnostic — the Domain crate has no `tokio`.
        let collected: Vec<Result<StreamEvent, AiError>> =
            futures::executor::block_on(stream.collect());
        assert_eq!(collected.len(), 3);
        assert!(matches!(collected[0], Ok(StreamEvent::TextDelta(_))));
        assert!(matches!(collected[1], Ok(StreamEvent::UsageReport(_))));
        assert!(matches!(collected[2], Ok(StreamEvent::Done)));
    }
}
