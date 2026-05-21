//! Streaming tool-call reassembler.
//!
//! [`ToolCallAccumulator`] is a pure state machine that turns a
//! stream of partial JSON fragments — as delivered by a streaming
//! provider's tool-call channel — into complete [`ToolCall`]
//! values. It carries no knowledge of HTTP, SSE framing, or any
//! provider's wire envelope; it accepts UTF-8 bytes and produces
//! Domain values.
//!
//! # Invariants the state machine guarantees
//!
//! - **Fragments may split on any character boundary**, including
//!   inside string literals and escape sequences. State is carried
//!   across [`ToolCallAccumulator::append`] calls so a fragment
//!   ending mid-string does not corrupt the parser.
//! - **Brace counting is string-aware.** A `}` inside a JSON string
//!   literal is not treated as a structural close. Backslash
//!   escapes inside strings consume the following code unit and
//!   keep the parser inside the string.
//! - **Multi-turn streams reuse the accumulator.** After
//!   [`ToolCallAccumulator::take_finished`] returns the assembled
//!   call, any bytes already buffered past the closing `}` resume
//!   the scan, so back-to-back tool calls in one stream surface as
//!   a sequence of `take_finished` results without losing data.
//! - **Interleaved prose is ignored.** Any bytes that arrive while
//!   the parser is outside a JSON object (no open brace yet, or
//!   between completed objects that have already been taken) are
//!   discarded. The provider adapter is expected to deliver only
//!   fragments belonging to a tool-call channel, but the
//!   accumulator stays defensive against accidental prose leaking
//!   in.
//! - **Malformed payloads never panic.** A JSON object whose
//!   structure balances but whose contents fail `serde_json` parsing
//!   surfaces as an [`AccumulatorError::InvalidJson`] and resets
//!   the internal state so the next fragment starts clean.
//!
//! # Why a hand-rolled scanner instead of `serde_json::de::Deserializer`
//!
//! The `serde_json` streaming deserialiser does not expose a "tell
//! me when this top-level value is structurally complete" hook
//! without buffering the whole input first, and it does not
//! tolerate arbitrary mid-fragment splits inside multi-byte
//! escape sequences. The state machine here pays the cost of a
//! ~50-line scanner in exchange for a pure, allocation-free hot
//! path that defers heavyweight parsing to exactly one
//! [`serde_json::from_str`] call per completed tool call.

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;
use thiserror::Error;

/// Fully assembled tool-call payload.
///
/// `id` is the provider-supplied correlation handle (the same value
/// the eventual `Part::ToolResult` echoes back as `call_id`).
/// `name` is the tool identifier. `arguments` is the parsed JSON
/// payload, retained as [`serde_json::Value`] because each tool
/// owns its own argument schema and the accumulator stays
/// schema-agnostic.
///
/// `Eq` is intentionally omitted — [`serde_json::Value`] only
/// implements `PartialEq` because of the `f64` it can carry, and
/// adding a wrapper just to recover `Eq` is more churn than the
/// downstream call sites need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolCall {
    /// Correlation handle the provider attached to the call.
    pub id: String,
    /// Tool identifier as advertised by the tool registry.
    pub name: String,
    /// Argument payload, schema owned by the named tool.
    pub arguments: serde_json::Value,
}

impl ToolCall {
    /// Construct a [`ToolCall`] from its three fields. Pairs with
    /// `#[non_exhaustive]` so adding a new field later (e.g. a
    /// provider-specific timing record) stays a non-breaking change
    /// for callers that go through the constructor.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

assert_impl_all!(ToolCall: Send, Sync);

/// Failure raised by [`ToolCallAccumulator::take_finished`].
///
/// Marked `#[non_exhaustive]` so future PRDs can add categories
/// (rate-limited mid-stream, schema mismatch reported by the
/// provider, …) without breaking matches in adapter code.
#[derive(Debug, Error, Clone)]
#[non_exhaustive]
pub enum AccumulatorError {
    /// Caller asked for a finished call before the brace stack
    /// returned to zero. The provider adapter should keep feeding
    /// fragments; this is not a recoverable error in the same
    /// sense as [`AccumulatorError::InvalidJson`] — the buffer
    /// stays intact and a later [`ToolCallAccumulator::take_finished`]
    /// will succeed once another fragment closes the object.
    #[error("no completed tool call is buffered yet")]
    NotComplete,

    /// The brace stack balanced but the resulting payload failed to
    /// parse as a JSON object with `id`, `name`, and `arguments`
    /// fields. The accumulator's state has been reset so the
    /// caller can resume on the next stream without leaking the
    /// failed buffer into the next call.
    #[error("malformed tool-call payload: {0}")]
    InvalidJson(String),
}

assert_impl_all!(AccumulatorError: Send, Sync);

/// Where the scanner currently sits inside the byte stream.
///
/// `Outside` covers prose between completed objects and any leading
/// bytes before the first `{`. `InObject` is the structural body of
/// a JSON object; the `depth` field tracks nesting so we can detect
/// when the outermost object closes. `InString` and `InStringEscape`
/// model the two string sub-states — inside a `"…"` literal the
/// scanner must ignore structural braces, and after a `\` it must
/// consume one further byte verbatim before re-entering the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    /// Outside any JSON object. Scanning for the next `{`.
    Outside,
    /// Inside a JSON object at the given brace depth (≥ 1), not
    /// currently inside a string literal.
    InObject { depth: u32 },
    /// Inside a `"…"` string literal at the given enclosing object
    /// depth.
    InString { depth: u32 },
    /// Inside a string literal, having just consumed a `\`. The
    /// next character is treated as a literal escape body and the
    /// scanner returns to [`ScanState::InString`] regardless of
    /// what that character is. This handles `\"`, `\\`, `\n`, and
    /// the leading byte of a `\uXXXX` escape — the four hex bytes
    /// that follow are plain `InString` characters and are
    /// harmless because none of them are `"`, `\`, `{`, or `}`.
    InStringEscape { depth: u32 },
}

/// State machine that reassembles a stream of partial JSON
/// fragments into [`ToolCall`] values.
///
/// The four-method surface (`new`, `append`, `is_complete`,
/// `take_finished`) is the contract every provider adapter
/// consumes. The internal scanner is allocation-free on the hot
/// path; the only allocation is the growing buffer that holds the
/// in-flight JSON object until it is parsed and discarded.
///
/// # Lifecycle
///
/// 1. Create with [`ToolCallAccumulator::new`].
/// 2. Feed fragments via [`ToolCallAccumulator::append`] as they
///    arrive from the provider stream.
/// 3. Poll [`ToolCallAccumulator::is_complete`] (or just call
///    `take_finished`, which surfaces `NotComplete` as a typed
///    error) to detect call boundaries.
/// 4. On completion, [`ToolCallAccumulator::take_finished`] returns
///    the [`ToolCall`] and resets internal state so the next
///    fragment starts a fresh call.
///
/// Multi-turn streams reuse the same accumulator — the scanner
/// remembers any bytes buffered past the closing `}` and resumes
/// scanning when `take_finished` is called.
#[derive(Debug, Clone)]
pub struct ToolCallAccumulator {
    buffer: String,
    scan_pos: usize,
    state: ScanState,
    object_start: Option<usize>,
    finished: Option<String>,
}

impl ToolCallAccumulator {
    /// Construct a fresh accumulator. The buffer is empty and the
    /// scanner is parked in the `Outside` state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            scan_pos: 0,
            state: ScanState::Outside,
            object_start: None,
            finished: None,
        }
    }

    /// Ingest one streamed fragment. The fragment is appended to
    /// the internal buffer and the scanner advances over the new
    /// bytes. Calling `append` repeatedly with concatenations of
    /// the same total bytes is equivalent to a single call with
    /// the joined string — the state machine is purely additive.
    ///
    /// Bytes that arrive while a finished call is still queued
    /// (see [`ToolCallAccumulator::is_complete`]) are buffered but
    /// not scanned until the queued call is drained via
    /// [`ToolCallAccumulator::take_finished`].
    pub fn append(&mut self, fragment: &str) {
        self.buffer.push_str(fragment);
        self.advance();
    }

    /// `true` when a JSON object's brace stack has returned to zero
    /// and the object is queued for parsing. The next call to
    /// [`ToolCallAccumulator::take_finished`] will succeed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.finished.is_some()
    }

    /// Drain the most recently completed JSON object into a
    /// [`ToolCall`].
    ///
    /// On success, the buffer is compacted (consumed bytes are
    /// dropped) and the scanner resumes from any remaining bytes
    /// — so a stream containing two back-to-back tool-call
    /// objects surfaces as two `take_finished` results without
    /// losing the prefix of the second object.
    ///
    /// # Errors
    ///
    /// Returns [`AccumulatorError::NotComplete`] when no object is
    /// queued. Returns [`AccumulatorError::InvalidJson`] when the
    /// queued bytes balance their braces but fail to parse as a
    /// `{ id, name, arguments }` shape; in that case internal
    /// state is reset so the caller can keep feeding the next
    /// stream without leaking the failed buffer.
    pub fn take_finished(&mut self) -> Result<ToolCall, AccumulatorError> {
        let raw = self.finished.take().ok_or(AccumulatorError::NotComplete)?;
        self.buffer.drain(..self.scan_pos);
        self.scan_pos = 0;

        let parsed =
            parse_tool_call(&raw).map_err(|err| AccumulatorError::InvalidJson(err.to_string()))?;

        self.advance();

        Ok(parsed)
    }

    fn advance(&mut self) {
        if self.finished.is_some() {
            return;
        }

        let start = self.scan_pos;
        let tail = &self.buffer[start..];

        for (rel_offset, ch) in tail.char_indices() {
            let abs_offset = start + rel_offset;

            match self.state {
                ScanState::Outside => {
                    if ch == '{' {
                        self.object_start = Some(abs_offset);
                        self.state = ScanState::InObject { depth: 1 };
                    }
                }
                ScanState::InObject { depth } => match ch {
                    '{' => {
                        self.state = ScanState::InObject {
                            depth: depth.saturating_add(1),
                        };
                    }
                    '}' => {
                        if depth == 1 {
                            let object_start = self
                                .object_start
                                .take()
                                .expect("object_start is Some while InObject");
                            let object_end = abs_offset + ch.len_utf8();
                            self.finished = Some(self.buffer[object_start..object_end].to_string());
                            self.scan_pos = object_end;
                            self.state = ScanState::Outside;
                            return;
                        }
                        self.state = ScanState::InObject { depth: depth - 1 };
                    }
                    '"' => {
                        self.state = ScanState::InString { depth };
                    }
                    _ => {}
                },
                ScanState::InString { depth } => match ch {
                    '\\' => {
                        self.state = ScanState::InStringEscape { depth };
                    }
                    '"' => {
                        self.state = ScanState::InObject { depth };
                    }
                    _ => {}
                },
                ScanState::InStringEscape { depth } => {
                    self.state = ScanState::InString { depth };
                }
            }

            self.scan_pos = abs_offset + ch.len_utf8();
        }
    }
}

impl Default for ToolCallAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

assert_impl_all!(ToolCallAccumulator: Send, Sync);

/// Parse a structurally-balanced JSON object into a [`ToolCall`].
///
/// Pulled out of [`ToolCallAccumulator::take_finished`] so the
/// failure path can be tested in isolation and so the validation
/// rules (id is a string, name is a string, arguments may be any
/// JSON value) live in exactly one place.
fn parse_tool_call(raw: &str) -> Result<ToolCall, ToolCallParseError> {
    #[derive(Deserialize)]
    struct Wire {
        id: String,
        name: String,
        #[serde(default)]
        arguments: serde_json::Value,
    }

    let wire: Wire = serde_json::from_str(raw).map_err(ToolCallParseError::Json)?;
    if wire.id.is_empty() {
        return Err(ToolCallParseError::EmptyId);
    }
    if wire.name.is_empty() {
        return Err(ToolCallParseError::EmptyName);
    }
    Ok(ToolCall::new(wire.id, wire.name, wire.arguments))
}

#[derive(Debug, Error)]
enum ToolCallParseError {
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("missing or empty `id` field")]
    EmptyId,
    #[error("missing or empty `name` field")]
    EmptyName,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_payload() -> &'static str {
        r#"{"id":"call_abc","name":"search","arguments":{"query":"a {nested} \"quote\" with \\ and \u00e9","limit":7,"nested":{"inner":true}}}"#
    }

    fn drive(chunks: &[&str]) -> ToolCall {
        let mut acc = ToolCallAccumulator::new();
        for chunk in chunks {
            acc.append(chunk);
        }
        assert!(
            acc.is_complete(),
            "stream did not complete after chunks {chunks:?}"
        );
        acc.take_finished().expect("payload parses cleanly")
    }

    #[test]
    fn new_accumulator_is_empty_and_not_complete() {
        let acc = ToolCallAccumulator::new();
        assert!(!acc.is_complete());
    }

    #[test]
    fn take_finished_before_any_append_reports_not_complete() {
        let mut acc = ToolCallAccumulator::new();
        let err = acc.take_finished().expect_err("nothing buffered yet");
        assert!(matches!(err, AccumulatorError::NotComplete));
    }

    #[test]
    fn clean_single_shot_payload_assembles() {
        let call = drive(&[reference_payload()]);
        assert_eq!(call.id, "call_abc");
        assert_eq!(call.name, "search");
        assert_eq!(
            call.arguments["query"],
            "a {nested} \"quote\" with \\ and é"
        );
        assert_eq!(call.arguments["limit"], 7);
        assert_eq!(call.arguments["nested"]["inner"], true);
    }

    #[test]
    fn fragments_split_mid_token_assemble() {
        let call = drive(&[
            r#"{"id":"#,
            r#""call_"#,
            r#"1","name":"#,
            r#""echo","arguments":{"#,
            r#""value":4"#,
            r#"2}}"#,
        ]);
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "echo");
        assert_eq!(call.arguments["value"], 42);
    }

    #[test]
    fn fragments_split_mid_string_assemble() {
        let call = drive(&[
            r#"{"id":"call_2","name":"note","arguments":{"text":"hello {wo"#,
            r#"rld} and \"quoted\" too"}}"#,
        ]);
        assert_eq!(call.id, "call_2");
        assert_eq!(call.arguments["text"], "hello {world} and \"quoted\" too");
    }

    #[test]
    fn fragments_split_mid_escape_assemble() {
        let call = drive(&[
            r#"{"id":"call_3","name":"path","arguments":{"value":"line1\"#,
            r#"nline2"}}"#,
        ]);
        assert_eq!(call.arguments["value"], "line1\nline2");
    }

    #[test]
    fn fragments_split_inside_unicode_escape_assemble() {
        let call = drive(&[r#"{"id":"u","name":"u","arguments":{"v":"\u00"#, r#"e9"}}"#]);
        assert_eq!(call.arguments["v"], "é");
    }

    #[test]
    fn multibyte_character_split_across_string_boundary() {
        let call = drive(&[r#"{"id":"m","name":"m","arguments":{"v":"caf"#, "é\"}}"]);
        assert_eq!(call.arguments["v"], "café");
    }

    #[test]
    fn multi_turn_back_to_back_calls() {
        let mut acc = ToolCallAccumulator::new();
        acc.append(
            r#"{"id":"a","name":"first","arguments":{}}{"id":"b","name":"second","arguments":{"k":1}}"#,
        );

        assert!(acc.is_complete());
        let first = acc.take_finished().unwrap();
        assert_eq!(first.id, "a");
        assert_eq!(first.name, "first");

        assert!(acc.is_complete());
        let second = acc.take_finished().unwrap();
        assert_eq!(second.id, "b");
        assert_eq!(second.arguments["k"], 1);

        assert!(!acc.is_complete());
    }

    #[test]
    fn multi_turn_with_split_between_calls() {
        let mut acc = ToolCallAccumulator::new();
        acc.append(r#"{"id":"a","name":"x","arguments":{}}"#);
        let first = acc.take_finished().unwrap();
        assert_eq!(first.id, "a");
        assert!(!acc.is_complete());

        acc.append(r#"{"id":"b","name":"#);
        assert!(!acc.is_complete());
        acc.append(r#""y","arguments":{"k":2}}"#);
        let second = acc.take_finished().unwrap();
        assert_eq!(second.id, "b");
        assert_eq!(second.arguments["k"], 2);
    }

    #[test]
    fn interleaved_prose_before_object_is_ignored() {
        let call = drive(&[
            "event: tool_call\ndata: ",
            r#"{"id":"i","name":"j","arguments":{}}"#,
        ]);
        assert_eq!(call.id, "i");
        assert_eq!(call.name, "j");
    }

    #[test]
    fn interleaved_prose_between_calls_is_ignored() {
        let mut acc = ToolCallAccumulator::new();
        acc.append(r#"{"id":"1","name":"n","arguments":{}}"#);
        acc.take_finished().unwrap();

        acc.append("\n\nignored prose here\n\n");
        assert!(!acc.is_complete());

        acc.append(r#"{"id":"2","name":"n","arguments":{}}"#);
        let next = acc.take_finished().unwrap();
        assert_eq!(next.id, "2");
    }

    #[test]
    fn malformed_json_surfaces_invalid_json_error() {
        let mut acc = ToolCallAccumulator::new();
        acc.append(r#"{"id":"x","arguments":{}}"#);
        assert!(acc.is_complete());
        let err = acc.take_finished().expect_err("should fail validation");
        assert!(matches!(err, AccumulatorError::InvalidJson(_)));
    }

    #[test]
    fn malformed_json_recovers_for_next_call() {
        let mut acc = ToolCallAccumulator::new();
        acc.append(r#"{"id":"","name":"n","arguments":{}}"#);
        let _ = acc.take_finished();

        acc.append(r#"{"id":"ok","name":"n","arguments":{"v":1}}"#);
        let next = acc.take_finished().expect("recovered");
        assert_eq!(next.id, "ok");
        assert_eq!(next.arguments["v"], 1);
    }

    #[test]
    fn empty_id_or_name_rejected() {
        let mut acc = ToolCallAccumulator::new();
        acc.append(r#"{"id":"","name":"x","arguments":{}}"#);
        assert!(matches!(
            acc.take_finished(),
            Err(AccumulatorError::InvalidJson(_))
        ));

        let mut acc = ToolCallAccumulator::new();
        acc.append(r#"{"id":"x","name":"","arguments":{}}"#);
        assert!(matches!(
            acc.take_finished(),
            Err(AccumulatorError::InvalidJson(_))
        ));
    }

    #[test]
    fn arguments_field_defaults_when_missing() {
        let mut acc = ToolCallAccumulator::new();
        acc.append(r#"{"id":"x","name":"y"}"#);
        let call = acc.take_finished().expect("missing arguments is fine");
        assert_eq!(call.id, "x");
        assert_eq!(call.arguments, serde_json::Value::Null);
    }

    #[test]
    fn one_byte_at_a_time_assembles_reference_payload() {
        let payload = reference_payload();
        let mut acc = ToolCallAccumulator::new();
        for ch in payload.chars() {
            let mut buf = [0u8; 4];
            acc.append(ch.encode_utf8(&mut buf));
        }
        assert!(acc.is_complete());
        let call = acc.take_finished().expect("char-by-char streaming works");
        assert_eq!(call.id, "call_abc");
        assert_eq!(call.name, "search");
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            ..proptest::test_runner::Config::default()
        })]

        #[test]
        fn random_splits_assemble_to_unsplit_reference(
            cuts in proptest::collection::vec(0usize..256, 0..32)
        ) {
            let payload = reference_payload();
            let reference = parse_tool_call(payload).expect("fixture is valid");

            let mut points: Vec<usize> = cuts
                .iter()
                .map(|c| {
                    let modded = c % payload.len().max(1);
                    payload
                        .char_indices()
                        .map(|(i, _)| i)
                        .chain(std::iter::once(payload.len()))
                        .find(|&i| i >= modded)
                        .unwrap_or(payload.len())
                })
                .collect();
            points.sort_unstable();
            points.dedup();

            let mut chunks: Vec<&str> = Vec::with_capacity(points.len() + 1);
            let mut prev = 0usize;
            for &p in &points {
                if p > prev {
                    chunks.push(&payload[prev..p]);
                    prev = p;
                }
            }
            if prev < payload.len() {
                chunks.push(&payload[prev..]);
            }

            let mut acc = ToolCallAccumulator::new();
            for chunk in &chunks {
                acc.append(chunk);
            }

            proptest::prop_assert!(acc.is_complete());
            let assembled = acc.take_finished().expect("assembled cleanly");
            proptest::prop_assert_eq!(assembled, reference);
        }
    }
}
