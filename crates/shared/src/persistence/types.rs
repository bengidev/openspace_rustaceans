//! Domain types the persistence trait surface references.
//!
//! `Workspace` already lives in [`crate::workspace`] and `Conversation`
//! / `Turn` already live in [`crate::ai::domain`]; this module fills
//! the remaining gaps so the trait surface can name every record it
//! reads or writes:
//!
//! - [`Session`] — a bounded interaction inside a workspace, scoped
//!   to a [`SessionMode`] (terminal, chat, editor). One workspace can
//!   carry many sessions.
//! - [`Attachment`] — a content-addressed blob the agent loop or a
//!   tool surfaced into a chat. Sized to round-trip through the
//!   future SQL backend without holding the bytes inline.
//! - [`PermissionGrant`] — an approval record the consent surface
//!   produces when the user allows a tool call (or a class of tool
//!   calls). Scoped per workspace, per session, or globally.
//!
//! Every shape carries the same hard rules the rest of the Domain
//! crate enforces: serde round-trippable, `Send + Sync` pinned at
//! compile time, `#[non_exhaustive]` so optional fields in later
//! slices stay non-breaking, and constructed via explicit `new`
//! methods rather than struct literals.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

use crate::id::{AttachmentId, ChatId, PermissionGrantId, SessionId, TurnId, WorkspaceId};

// ─────────────────────────────────────────────────────────────────────
// SessionMode + Session — a bounded interaction inside a workspace.
// ─────────────────────────────────────────────────────────────────────

/// Operating surface a [`Session`] is bound to.
///
/// Mirrors the three modes [`crate`]-level docs and `CONTEXT.md`
/// enumerate. Persistence stores the discriminant so the workspace
/// switcher can render a session next to the right mode pane on
/// resume.
///
/// Closed set on purpose — every product surface is one of these
/// three. Adding a new mode is a deliberate schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Interactive shell with an AI sidekick.
    Terminal,
    /// Long-form conversational surface.
    Chat,
    /// Text/code editor with inline AI assistance.
    Editor,
}

assert_impl_all!(SessionMode: Send, Sync);

/// Bounded interaction inside a workspace.
///
/// A session bundles the metadata the workspace switcher and the
/// recents list need without forcing the full conversation /
/// terminal-pane / buffer payload to load up front. The heavy bits
/// live in mode-specific records; persistence resolves them on
/// demand through the matching repository.
///
/// `#[non_exhaustive]` so PRD-03 can extend the record (pinning,
/// archival flag, rolling token budget) without churning struct
/// literals downstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Session {
    /// Stable identity. Survives renames and mode-pane reshuffles.
    pub id: SessionId,
    /// Owning workspace. A session never outlives its workspace —
    /// deleting a workspace cascades to its sessions.
    pub workspace_id: WorkspaceId,
    /// Operating surface this session is bound to. Persistence keys
    /// the recents list off this so the switcher can render a
    /// terminal session next to its terminal-mode siblings.
    pub mode: SessionMode,
    /// Optional human-friendly title. `None` until the user names the
    /// session or the assistant suggests one.
    pub title: Option<String>,
    /// First-write timestamp. Locked at session creation.
    pub created_at: DateTime<Utc>,
    /// Most-recent-write timestamp. Updated whenever the session's
    /// canonical state changes (a turn lands, the title is renamed,
    /// the mode-specific payload is overwritten).
    pub updated_at: DateTime<Utc>,
}

impl Session {
    /// Construct a session from its core fields.
    ///
    /// Wrapping struct construction in a method keeps the
    /// `#[non_exhaustive]` guarantee meaningful: callers cannot
    /// accidentally depend on the field set, so adding a new
    /// optional field later is non-breaking.
    #[must_use]
    pub fn new(
        id: SessionId,
        workspace_id: WorkspaceId,
        mode: SessionMode,
        title: Option<String>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            workspace_id,
            mode,
            title,
            created_at,
            updated_at,
        }
    }
}

assert_impl_all!(Session: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Attachment — content-addressed blob metadata.
// ─────────────────────────────────────────────────────────────────────

/// Metadata record for a content-addressed attachment.
///
/// The blob bytes themselves live in the on-disk store the
/// [`super::repository::AttachmentStore`] manages; this struct is the
/// Domain handle every other record uses to refer to the blob without
/// holding the bytes inline.
///
/// `content_hash` is opaque on purpose — the format (sha256, blake3,
/// …) is an implementation detail of the store. Every consumer
/// treats it as a stable lookup key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Attachment {
    /// Stable identity. Independent of `content_hash` so two
    /// attachments backed by the same blob bytes (e.g. the same file
    /// pasted into two chats) keep distinct provenance.
    pub id: AttachmentId,
    /// Owning chat. Attachments are scoped to chats today; if PRD-03
    /// later attaches blobs to terminal panes or buffers, this field
    /// becomes optional.
    pub chat_id: ChatId,
    /// Optional turn the attachment is anchored to. `None` for
    /// session-scoped attachments uploaded ahead of any turn.
    pub turn_id: Option<TurnId>,
    /// Human-readable filename, used for rendering. Not required to
    /// be unique.
    pub filename: String,
    /// MIME type as classified at ingest time. Stored alongside the
    /// blob so the renderer does not have to re-sniff every time.
    pub mime_type: String,
    /// Size of the underlying blob in bytes. Stored on the metadata
    /// record so the recents UI can render a size hint without
    /// touching the store.
    pub size_bytes: u64,
    /// Content-addressed lookup key for the blob bytes. Format is an
    /// implementation detail of the store; consumers treat it as a
    /// stable string handle.
    pub content_hash: String,
    /// First-write timestamp. Pinned at ingest.
    pub created_at: DateTime<Utc>,
}

impl Attachment {
    /// Construct an attachment metadata record. Pairs with
    /// `#[non_exhaustive]` for the same reason every other Domain
    /// type in this crate has an explicit constructor.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: AttachmentId,
        chat_id: ChatId,
        turn_id: Option<TurnId>,
        filename: String,
        mime_type: String,
        size_bytes: u64,
        content_hash: String,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            chat_id,
            turn_id,
            filename,
            mime_type,
            size_bytes,
            content_hash,
            created_at,
        }
    }
}

assert_impl_all!(Attachment: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// PermissionScope + PermissionDecision + PermissionGrant — consent
// surface output.
// ─────────────────────────────────────────────────────────────────────

/// Scope a permission grant covers.
///
/// Mirrors the three consent surfaces the agent loop offers: "just
/// this call", "for this session", "for this workspace". The values
/// are ordered by widening blast radius — a settings UI that renders
/// the choices in this order matches the user's mental model.
///
/// Closed set on purpose. A future "global / forever" scope is
/// intentionally absent: PRD-03 keeps consent workspace-bounded so
/// switching folders never inherits permissions silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    /// Single tool call. Consumed once and discarded.
    SingleCall,
    /// Every matching tool call inside the originating session, until
    /// the session ends.
    Session,
    /// Every matching tool call inside the originating workspace,
    /// until the user revokes the grant.
    Workspace,
}

assert_impl_all!(PermissionScope: Send, Sync);

/// Decision the user (or a fall-through default) attached to a grant.
///
/// `Allow` and `Deny` are symmetric — both are first-class records so
/// the consent UI can render a "you previously declined this" hint
/// without re-prompting on every call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// The user allowed the matching tool calls.
    Allow,
    /// The user denied the matching tool calls.
    Deny,
}

assert_impl_all!(PermissionDecision: Send, Sync);

/// Permission grant produced by the consent surface.
///
/// `tool_id` is matched against [`crate::tool`]'s identifier; future
/// PRDs may extend the match shape (parameter predicates, path
/// prefixes) — `#[non_exhaustive]` keeps that growth non-breaking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PermissionGrant {
    /// Stable identity. Lets the revoke flow target a specific grant
    /// without echoing the whole tuple.
    pub id: PermissionGrantId,
    /// Owning workspace. Grants never cross workspace boundaries.
    pub workspace_id: WorkspaceId,
    /// Originating session — `None` only when the grant scope is
    /// [`PermissionScope::Workspace`] and the user issued the grant
    /// from the settings surface rather than a live consent prompt.
    pub session_id: Option<SessionId>,
    /// Identifier of the tool the grant covers. Matches
    /// [`crate::tool`]'s naming.
    pub tool_id: String,
    /// Scope the grant applies in.
    pub scope: PermissionScope,
    /// Allow or deny.
    pub decision: PermissionDecision,
    /// Free-form note the user (or the consent surface) attached.
    /// Surfaced in the audit log so a later reviewer can recover the
    /// reasoning behind the grant.
    pub note: Option<String>,
    /// First-write timestamp. Pinned at grant creation.
    pub created_at: DateTime<Utc>,
}

impl PermissionGrant {
    /// Construct a permission grant. Pairs with `#[non_exhaustive]`
    /// for the usual reason.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PermissionGrantId,
        workspace_id: WorkspaceId,
        session_id: Option<SessionId>,
        tool_id: String,
        scope: PermissionScope,
        decision: PermissionDecision,
        note: Option<String>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            workspace_id,
            session_id,
            tool_id,
            scope,
            decision,
            note,
            created_at,
        }
    }
}

assert_impl_all!(PermissionGrant: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Path-related helpers used by the recent-workspaces repository to
// surface a workspace's on-disk root alongside its handle. Lives here
// because the Domain crate already pulls `PathBuf` for the workspace
// record — keeping the import close to its only user makes the seam
// obvious for downstream slices.
// ─────────────────────────────────────────────────────────────────────

#[doc(hidden)]
#[allow(dead_code)]
pub(crate) fn _path_buf_marker() -> PathBuf {
    PathBuf::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        // `parse_from_rfc3339` keeps the test deterministic — using
        // `Utc::now()` would make round-trip diagnostics noisy.
        DateTime::parse_from_rfc3339("2026-05-22T03:00:00Z")
            .expect("parse")
            .with_timezone(&Utc)
    }

    #[test]
    fn session_round_trips_through_json() {
        let original = Session::new(
            SessionId::new_v4(),
            WorkspaceId::new_v4(),
            SessionMode::Chat,
            Some("Welcome chat".to_string()),
            now(),
            now(),
        );
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Session = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn attachment_round_trips_through_json() {
        let original = Attachment::new(
            AttachmentId::new_v4(),
            ChatId::new_v4(),
            Some(TurnId::new_v4()),
            "spec.pdf".to_string(),
            "application/pdf".to_string(),
            12_345,
            "blake3-deadbeef".to_string(),
            now(),
        );
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Attachment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn permission_grant_round_trips_through_json() {
        let original = PermissionGrant::new(
            PermissionGrantId::new_v4(),
            WorkspaceId::new_v4(),
            Some(SessionId::new_v4()),
            "fs.read".to_string(),
            PermissionScope::Session,
            PermissionDecision::Allow,
            Some("trusted workspace".to_string()),
            now(),
        );
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: PermissionGrant = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// Lock the snake-case wire shape for the enum discriminants —
    /// future telemetry pipelines key off these strings.
    #[test]
    fn enum_wire_shapes_are_snake_case() {
        let mode = serde_json::to_string(&SessionMode::Editor).expect("serialize");
        assert_eq!(mode, "\"editor\"");

        let scope = serde_json::to_string(&PermissionScope::SingleCall).expect("serialize");
        assert_eq!(scope, "\"single_call\"");

        let decision = serde_json::to_string(&PermissionDecision::Deny).expect("serialize");
        assert_eq!(decision, "\"deny\"");
    }
}
