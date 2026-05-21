//! Server-Sent Events parser tuned for the streaming chat-completions
//! wire format.
//!
//! The SSE spec (`text/event-stream`) defines a tiny line-oriented
//! framing: lines starting with `data:` carry the event payload,
//! blank lines flush the accumulated payload as one event, lines
//! starting with `:` are comments, and `event:` / `id:` / `retry:`
//! are metadata we do not consume on this channel.
//!
//! [`SseParser`] is a pure state machine that ingests arbitrary
//! [`bytes::Bytes`] chunks (as delivered by the sandbox HTTP
//! client's streaming response) and yields complete `data:`
//! payloads. The parser tolerates chunk boundaries falling anywhere
//! — mid-line, mid-multibyte UTF-8 sequence, between `\r` and `\n`
//! in a CRLF separator — because the underlying transport makes no
//! promise that a chunk maps to a frame.
//!
//! Behaviour the issue's acceptance criteria pin:
//!
//! - Blank lines flush the buffer as a single event boundary.
//! - Comment lines (`:` prefix) are dropped entirely.
//! - `data:` payloads are concatenated across consecutive `data:`
//!   lines belonging to the same event, separated by `\n` per the
//!   spec.
//! - Partial chunks split mid-frame are buffered until the next
//!   chunk arrives.
//!
//! The terminator for the chat stream is a payload containing the
//! literal string `[DONE]`. The parser treats it as an ordinary
//! event; the consumer in [`super::provider`] is responsible for
//! recognising it and stopping the stream.

/// Streaming SSE parser.
///
/// Consumes byte chunks via [`SseParser::feed`] and produces complete
/// `data:` payloads via [`SseParser::next_event`]. Allocations:
///
/// - One `Vec<u8>` for the cross-chunk byte buffer.
/// - One `String` per event boundary while the payload is being
///   reassembled.
///
/// Both are reused across events; only the payload `String` is
/// handed to the caller.
#[derive(Debug, Default)]
pub(super) struct SseParser {
    /// Bytes accumulated since the last newline. The parser splits
    /// on every newline it sees and feeds the line into the line
    /// processor.
    line_buffer: Vec<u8>,
    /// Payload accumulated across consecutive `data:` lines belonging
    /// to the same event. Drained when a blank line flushes the
    /// event.
    pending_data: String,
    /// Completed events ready for [`Self::next_event`] to drain. Kept
    /// as a small queue so a single `feed` call that contains
    /// multiple events still surfaces them one by one.
    completed: std::collections::VecDeque<String>,
    /// Tracks whether the previous byte was a `\r`. SSE allows
    /// `\r`, `\n`, and `\r\n` as line terminators; this flag lets us
    /// collapse `\r\n` into a single boundary without misreading a
    /// lone `\r`.
    saw_cr: bool,
}

impl SseParser {
    /// Construct a fresh parser with empty buffers.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Ingest a byte chunk. Bytes are appended to the cross-chunk
    /// buffer and any newly-completed events are queued for
    /// [`Self::next_event`] to drain.
    pub(super) fn feed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            match (byte, self.saw_cr) {
                // CRLF — saw `\r`, now `\n`. The `\r` already
                // terminated a line, so swallow this `\n` as the
                // continuation of the same separator.
                (b'\n', true) => {
                    self.saw_cr = false;
                }
                // Lone `\n` — terminate a line.
                (b'\n', false) => {
                    self.flush_line();
                }
                // Any byte after a `\r` that is not `\n` —
                // the previous `\r` was a line terminator on its
                // own; this byte starts a new line.
                (other, true) => {
                    self.saw_cr = false;
                    if other == b'\r' {
                        // `\r\r` — the first `\r` already terminated
                        // a line, the second one is itself a
                        // terminator.
                        self.flush_line();
                    } else {
                        self.line_buffer.push(other);
                    }
                }
                // `\r` — defer; we do not yet know whether a `\n`
                // follows.
                (b'\r', false) => {
                    self.saw_cr = true;
                    self.flush_line();
                }
                // Ordinary byte — append to the pending line.
                (other, false) => {
                    self.line_buffer.push(other);
                }
            }
        }
    }

    /// Drain the next completed event, if any. Returns `None` when
    /// the parser is mid-event and the caller should feed more
    /// bytes.
    pub(super) fn next_event(&mut self) -> Option<String> {
        self.completed.pop_front()
    }

    /// Process the line currently in [`Self::line_buffer`] and reset
    /// the buffer for the next line.
    fn flush_line(&mut self) {
        // Owning `take` lets us avoid borrowing `self` mutably twice
        // when `pending_data` is also `&mut self`.
        let raw = std::mem::take(&mut self.line_buffer);

        // Empty line — flush the accumulated payload as one event.
        if raw.is_empty() {
            if !self.pending_data.is_empty() {
                let event = std::mem::take(&mut self.pending_data);
                self.completed.push_back(event);
            }
            return;
        }

        // SSE is defined as UTF-8. A non-UTF-8 line is a protocol
        // violation; lossy conversion keeps the parser progressing
        // and surfaces the bad bytes as `U+FFFD` for the consumer
        // to flag if it cares.
        let line = String::from_utf8_lossy(&raw);

        // Comment line — drop entirely.
        if line.starts_with(':') {
            return;
        }

        // `data:` payload — append to the pending event. Per spec,
        // a single leading space after the colon is stripped; the
        // rest of the line (including any further whitespace) is
        // preserved verbatim.
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            if !self.pending_data.is_empty() {
                self.pending_data.push('\n');
            }
            self.pending_data.push_str(payload);
        }

        // Other framing lines (`event:`, `id:`, `retry:`) are
        // ignored for this channel. The streaming chat-completions
        // wire format only uses `data:` events.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(parser: &mut SseParser) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(event) = parser.next_event() {
            out.push(event);
        }
        out
    }

    #[test]
    fn single_complete_event_is_returned() {
        let mut p = SseParser::new();
        p.feed(b"data: hello\n\n");
        assert_eq!(drain(&mut p), vec!["hello".to_string()]);
    }

    #[test]
    fn crlf_line_endings_are_treated_the_same_as_lf() {
        let mut p = SseParser::new();
        p.feed(b"data: hello\r\n\r\n");
        assert_eq!(drain(&mut p), vec!["hello".to_string()]);
    }

    #[test]
    fn comment_lines_are_dropped() {
        let mut p = SseParser::new();
        p.feed(b": keepalive\ndata: hi\n\n");
        assert_eq!(drain(&mut p), vec!["hi".to_string()]);
    }

    #[test]
    fn multiple_data_lines_concatenate_with_newline() {
        let mut p = SseParser::new();
        p.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(drain(&mut p), vec!["line1\nline2".to_string()]);
    }

    #[test]
    fn partial_chunks_split_mid_line_buffer_and_resume() {
        let mut p = SseParser::new();
        p.feed(b"data: hel");
        assert!(drain(&mut p).is_empty());
        p.feed(b"lo\n");
        assert!(drain(&mut p).is_empty());
        p.feed(b"\n");
        assert_eq!(drain(&mut p), vec!["hello".to_string()]);
    }

    #[test]
    fn partial_chunk_split_between_cr_and_lf() {
        let mut p = SseParser::new();
        p.feed(b"data: hi\r");
        // Splitting between `\r` and `\n` must not produce a spurious
        // event: the parser defers until the next byte arrives.
        assert!(drain(&mut p).is_empty());
        p.feed(b"\n\r\n");
        assert_eq!(drain(&mut p), vec!["hi".to_string()]);
    }

    #[test]
    fn back_to_back_events_in_one_chunk_surface_separately() {
        let mut p = SseParser::new();
        p.feed(b"data: a\n\ndata: b\n\n");
        assert_eq!(drain(&mut p), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn other_framing_lines_are_ignored() {
        let mut p = SseParser::new();
        p.feed(b"event: chunk\nid: 1\nretry: 1000\ndata: payload\n\n");
        assert_eq!(drain(&mut p), vec!["payload".to_string()]);
    }

    #[test]
    fn blank_line_without_data_is_no_op() {
        let mut p = SseParser::new();
        p.feed(b"\n\n\n");
        assert!(drain(&mut p).is_empty());
    }

    #[test]
    fn data_prefix_without_leading_space_keeps_full_payload() {
        // Per spec, only one leading space after the colon is
        // stripped — anything else is preserved.
        let mut p = SseParser::new();
        p.feed(b"data:no-space\n\n");
        assert_eq!(drain(&mut p), vec!["no-space".to_string()]);
    }

    #[test]
    fn done_sentinel_is_returned_verbatim() {
        // The parser does not interpret `[DONE]` — that is the
        // consumer's job. Pinning the contract here keeps the
        // boundary clear.
        let mut p = SseParser::new();
        p.feed(b"data: [DONE]\n\n");
        assert_eq!(drain(&mut p), vec!["[DONE]".to_string()]);
    }
}
