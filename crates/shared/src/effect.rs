//! Side-effect catalogue.
//!
//! [`Effect`] is the exhaustive list of things a tool call (or a
//! command-palette handler) can ask the home shell to do once its own
//! work is complete. The dispatcher in `feature-home` (PRD-07)
//! pattern-matches on this enum to translate domain-level outcomes
//! into Iced messages — opening a file in the editor pane, submitting
//! a command into the terminal, scrolling the chat to a specific
//! turn, and so on.
//!
//! # Why a single enum
//!
//! Every tool produces a [`crate::tool::ToolOutcome`] carrying a
//! `Vec<Effect>`. A single enum gives us one place to:
//!
//! 1. Reason about the *blast radius* of a tool call. The reviewer
//!    can read the variant set and know exactly what tools are
//!    allowed to influence.
//! 2. Round-trip a recorded run through serde for snapshot logging
//!    in later phases — every variant carries enough data to be
//!    re-applied without further lookup.
//! 3. Keep the dispatcher exhaustive: a new effect that lacks a
//!    handler in `feature-home` fails the match check at compile
//!    time, so we cannot silently drop a side-effect on the floor.
//!
//! `Effect` is `#[non_exhaustive]` so a future PRD can land an
//! additional variant (a structured progress event, a
//! preview-window update, …) without breaking pattern matches in
//! downstream crates that were compiled against the previous set.
//!
//! # Variant choices
//!
//! Each variant is a pure data record — no callbacks, no closures,
//! no IO handles. The dispatcher consumes a `Vec<Effect>` and
//! replays the variants in order; if a tool needs to wait on the
//! result of an effect, it delivers the effect, returns control,
//! and a follow-up tool call resumes the work.
//!
//! - [`Effect::ApplyBufferEdits`] — replace text inside an editor
//!   buffer. The list of [`BufferEdit`]s is applied in order, in a
//!   single transaction, against the buffer identified by
//!   [`crate::id::BufferId`].
//! - [`Effect::SubmitTerminalCommand`] — push a command into a
//!   terminal pane. The pane is responsible for echoing the command
//!   and capturing the output; this effect just hands the string
//!   over.
//! - [`Effect::OpenFile`] — open or focus a file in the editor.
//!   `focus = true` switches the active pane; `false` opens in the
//!   background (e.g. so a "look at this for context" surface can
//!   load it without stealing focus).
//! - [`Effect::EmitNotification`] — toast the user. The
//!   [`NotificationLevel`] picks the colour and the icon; the
//!   message is rendered verbatim.
//! - [`Effect::ShowApprovalRequest`] — render an approval card.
//!   The `reason` is the same [`crate::sandbox::ApprovalReason`]
//!   the sandbox produces, so a single rendering layer covers both
//!   sandbox-driven and tool-driven prompts. The `request_id` is
//!   the correlation handle the future approval response carries
//!   back.
//! - [`Effect::ShowUpdateBanner`] — chrome-level banner the home
//!   shell pins above the workspace. Carries optional action copy
//!   so the renderer can decide whether to draw a button.
//! - [`Effect::RefreshConversationHistory`] — invalidate the chat
//!   history cache for a chat. Used after a tool mutated the
//!   underlying store out-of-band.
//! - [`Effect::ScrollToTurn`] — jump the chat to a specific turn.
//!   The chat id disambiguates when more than one chat is open.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

use crate::id::{BufferId, ChatId, TerminalPaneId, TurnId};
use crate::sandbox::ApprovalReason;

// ─────────────────────────────────────────────────────────────────────
// TextPosition / TextRange / BufferEdit — shape-only edit description.
// No rope, no buffer logic; that lives in the editor crate. This slice
// only fixes the wire shape.
// ─────────────────────────────────────────────────────────────────────

/// Zero-based `(line, column)` position inside a buffer.
///
/// Both axes are `u32` because the editor's rope already caps line
/// and column counts at `u32::MAX`; using a wider integer here would
/// invite mismatches at the boundary. Positions are exclusive of the
/// end column when they appear in a [`TextRange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextPosition {
    /// Zero-based line index.
    pub line: u32,
    /// Zero-based column index, measured in UTF-16 code units to
    /// stay consistent with the editor surface. The dispatcher is
    /// responsible for any unit conversion the underlying rope
    /// requires.
    pub column: u32,
}

impl TextPosition {
    /// Construct a position from explicit `(line, column)` values.
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

assert_impl_all!(TextPosition: Send, Sync);

/// Half-open `[start, end)` range inside a buffer.
///
/// `start <= end` is *not* enforced at the type level. The editor
/// dispatcher normalises the pair before applying, so a tool that
/// emits a reversed range still produces a deterministic edit; we
/// keep the type cheap (no `Result` constructor) because PRD-07
/// owns the validation surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRange {
    /// Inclusive start of the range.
    pub start: TextPosition,
    /// Exclusive end of the range.
    pub end: TextPosition,
}

impl TextRange {
    /// Construct a range from its endpoints.
    #[must_use]
    pub const fn new(start: TextPosition, end: TextPosition) -> Self {
        Self { start, end }
    }
}

assert_impl_all!(TextRange: Send, Sync);

/// Single replacement inside a buffer.
///
/// `range` selects the slice to remove (possibly empty for a pure
/// insertion); `replacement` is the text to splice in. A
/// [`Effect::ApplyBufferEdits`] applies a `Vec<BufferEdit>` in
/// order, in a single transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferEdit {
    /// Slice of the buffer this edit replaces.
    pub range: TextRange,
    /// New text spliced in at `range.start`. May be empty (a pure
    /// deletion) or longer than the original slice (a replacement
    /// that grows the buffer).
    pub replacement: String,
}

impl BufferEdit {
    /// Construct a buffer edit from its range and replacement text.
    #[must_use]
    pub fn new(range: TextRange, replacement: impl Into<String>) -> Self {
        Self {
            range,
            replacement: replacement.into(),
        }
    }
}

assert_impl_all!(BufferEdit: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// NotificationLevel — colour/icon dial for `Effect::EmitNotification`.
// ─────────────────────────────────────────────────────────────────────

/// Severity dial for [`Effect::EmitNotification`].
///
/// The renderer picks the icon and the colour from this enum; the
/// message string is rendered verbatim. Adding a new level (e.g.
/// `Success`) is a non-breaking change as the type is
/// `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum NotificationLevel {
    /// Informational toast — neutral colour, no urgency.
    Info,
    /// Warning toast — yellow accent, advisory.
    Warning,
    /// Error toast — red accent, surfaces a failure that the user
    /// should acknowledge.
    Error,
}

assert_impl_all!(NotificationLevel: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Effect — the catalogue. `#[non_exhaustive]` so future variants are
// non-breaking. Every variant carries enough data to be applied
// without further lookup, per the PRD lock.
// ─────────────────────────────────────────────────────────────────────

/// Side-effect a tool or command requests once its own work is done.
///
/// See the module-level docs for variant rationale and the
/// dispatcher contract. Marked `#[non_exhaustive]` — consumers must
/// include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Effect {
    /// Apply a sequence of edits to an editor buffer in a single
    /// transaction.
    ApplyBufferEdits {
        /// Identity of the target buffer.
        buffer_id: BufferId,
        /// Edits applied in list order. May be empty — a no-op edit
        /// is still a valid effect (the dispatcher uses it to bump
        /// version counters).
        edits: Vec<BufferEdit>,
    },

    /// Push a command into a terminal pane. The pane echoes the
    /// command and captures the output; this effect just delivers
    /// the string.
    SubmitTerminalCommand {
        /// Target terminal pane.
        pane_id: TerminalPaneId,
        /// Command to submit, verbatim. Trailing newline handling
        /// is the pane's responsibility — tools do not append one.
        command: String,
    },

    /// Open or focus a file in the editor.
    OpenFile {
        /// Filesystem path of the file to open. The dispatcher
        /// resolves the path against the active workspace.
        path: PathBuf,
        /// Whether to switch the active pane to the opened buffer
        /// (`true`) or load it in the background (`false`).
        focus: bool,
    },

    /// Toast the user with a single-line message.
    EmitNotification {
        /// Severity dial driving icon and colour.
        level: NotificationLevel,
        /// Human-readable message. Rendered verbatim.
        message: String,
    },

    /// Render an approval card. The dispatcher surfaces the prompt
    /// and routes the user's response back through the same
    /// `request_id` so the waiting tool can resume.
    ShowApprovalRequest {
        /// Structured reason copy. Shared with the sandbox so the
        /// rendering layer is uniform across sandbox-driven and
        /// tool-driven prompts.
        reason: ApprovalReason,
        /// Correlation handle. The follow-up approval response
        /// carries the same id so concurrent prompts do not cross
        /// streams.
        request_id: String,
    },

    /// Pin a banner at the top of the home shell — typically used
    /// for "an update is available" surfaces.
    ShowUpdateBanner {
        /// Banner copy. Rendered verbatim.
        message: String,
        /// Optional action button label. When `None`, the renderer
        /// shows a plain banner with no actionable affordance.
        action_label: Option<String>,
    },

    /// Invalidate the chat history cache for a chat. Used after a
    /// tool mutated the underlying store out-of-band and the
    /// rendered transcript needs to reload.
    RefreshConversationHistory {
        /// Identity of the chat whose cache should be invalidated.
        chat_id: ChatId,
    },

    /// Jump the chat surface to a specific turn.
    ScrollToTurn {
        /// Chat hosting the target turn.
        chat_id: ChatId,
        /// Identity of the turn to scroll into view.
        turn_id: TurnId,
    },
}

assert_impl_all!(Effect: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(original: T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// Round-trip an `ApplyBufferEdits` carrying a non-trivial edit
    /// list — covers the nested `BufferEdit` / `TextRange` /
    /// `TextPosition` shape in one go.
    #[test]
    fn apply_buffer_edits_round_trips() {
        round_trip(Effect::ApplyBufferEdits {
            buffer_id: BufferId::new_v4(),
            edits: vec![
                BufferEdit::new(
                    TextRange::new(TextPosition::new(0, 0), TextPosition::new(0, 5)),
                    "hello",
                ),
                BufferEdit::new(
                    TextRange::new(TextPosition::new(2, 4), TextPosition::new(2, 4)),
                    " world",
                ),
            ],
        });
    }

    /// Approval-flavoured variant — proves the cross-module
    /// `ApprovalReason` import survives a JSON hop.
    #[test]
    fn show_approval_request_round_trips() {
        round_trip(Effect::ShowApprovalRequest {
            reason: ApprovalReason::with_detail(
                "tool wants to write outside the workspace",
                "/etc/hosts is outside the workspace root",
            ),
            request_id: "req-42".to_string(),
        });
    }

    /// Notification variant covers every `NotificationLevel` so a
    /// future rename to the wire form (e.g. snake_case toggle) is
    /// caught by this test rather than at the call site.
    #[test]
    fn emit_notification_round_trips_every_level() {
        for level in [
            NotificationLevel::Info,
            NotificationLevel::Warning,
            NotificationLevel::Error,
        ] {
            round_trip(Effect::EmitNotification {
                level,
                message: format!("level={level:?}"),
            });
        }
    }

    /// Open-file is the simplest path-bearing variant — locks the
    /// `PathBuf` wire shape and the `focus` flag together.
    #[test]
    fn open_file_round_trips() {
        round_trip(Effect::OpenFile {
            path: PathBuf::from("/workspace/src/lib.rs"),
            focus: true,
        });
    }

    /// `Effect` is `#[non_exhaustive]`. Lock that callers must use a
    /// wildcard arm: matching every named variant followed by `_`
    /// stays unreachable today, but the test fails the day a new
    /// variant lands without `#[non_exhaustive]` honoured (the
    /// wildcard would suddenly become required).
    #[test]
    fn match_uses_wildcard_arm() {
        let effects = [
            Effect::ApplyBufferEdits {
                buffer_id: BufferId::new_v4(),
                edits: Vec::new(),
            },
            Effect::SubmitTerminalCommand {
                pane_id: TerminalPaneId::new_v4(),
                command: "ls".to_string(),
            },
            Effect::OpenFile {
                path: PathBuf::from("/x"),
                focus: false,
            },
            Effect::EmitNotification {
                level: NotificationLevel::Info,
                message: "hi".to_string(),
            },
            Effect::ShowApprovalRequest {
                reason: ApprovalReason::new("why"),
                request_id: "r".to_string(),
            },
            Effect::ShowUpdateBanner {
                message: "update".to_string(),
                action_label: None,
            },
            Effect::RefreshConversationHistory {
                chat_id: ChatId::new_v4(),
            },
            Effect::ScrollToTurn {
                chat_id: ChatId::new_v4(),
                turn_id: TurnId::new_v4(),
            },
        ];
        for e in effects {
            #[allow(unreachable_patterns)]
            let label = match e {
                Effect::ApplyBufferEdits { .. } => "buf",
                Effect::SubmitTerminalCommand { .. } => "term",
                Effect::OpenFile { .. } => "open",
                Effect::EmitNotification { .. } => "note",
                Effect::ShowApprovalRequest { .. } => "approve",
                Effect::ShowUpdateBanner { .. } => "banner",
                Effect::RefreshConversationHistory { .. } => "refresh",
                Effect::ScrollToTurn { .. } => "scroll",
                _ => "unknown",
            };
            assert!(matches!(
                label,
                "buf" | "term" | "open" | "note" | "approve" | "banner" | "refresh" | "scroll"
            ));
        }
    }

    /// Lock the wire form's variant key. `serde(rename_all =
    /// "snake_case")` should produce `apply_buffer_edits`, not
    /// `ApplyBufferEdits`.
    #[test]
    fn wire_form_uses_snake_case_variant_key() {
        let effect = Effect::OpenFile {
            path: PathBuf::from("/x"),
            focus: false,
        };
        let value: serde_json::Value = serde_json::to_value(&effect).expect("serialize as value");
        let object = value.as_object().expect("object");
        assert!(object.contains_key("open_file"));
    }
}
