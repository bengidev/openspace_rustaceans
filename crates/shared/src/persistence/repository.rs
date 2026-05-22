//! Repository / store trait surface — the seven traits PRD-03 ships.
//!
//! Every trait below describes what callers can ask of the persistence
//! layer. Concrete impls live on `openspace-persistence` and arrive in
//! follow-up slices; this slice ships signatures only so consumers
//! (chat mode, recents, settings) can start writing against the trait
//! shape immediately.
//!
//! # Trait inventory
//!
//! - [`WorkspaceRepository`] — workspace CRUD.
//! - [`SessionRepository`] — sessions inside a workspace.
//! - [`ChatRepository`] — chats inside a session.
//! - [`TurnRepository`] — turns inside a chat.
//! - [`AttachmentStore`] — content-addressed blob metadata + bytes.
//! - [`PermissionGrantStore`] — consent-surface grants per workspace.
//! - [`RecentWorkspacesRepository`] — recents bookkeeping for the
//!   workspace switcher.
//!
//! # Trait shape
//!
//! Methods are `async fn` via `async-trait`. The macro keeps the
//! traits `dyn`-compatible (object-safe) so the agent loop can hold
//! `Arc<dyn WorkspaceRepository>` and friends — the same pattern
//! [`crate::ai::provider::AiProvider`] uses. Every trait pins
//! `Send + Sync` at the trait bound *and* via
//! `assert_impl_all!(dyn Trait: Send, Sync)` on the trait object so a
//! future change cannot quietly regress the sharing pattern.
//!
//! # Why this many traits
//!
//! PRD-03 splits the storage surface along the same seams the rest
//! of the Domain crate uses. Three benefits:
//!
//!   1. Feature crates depend on the narrowest trait they need —
//!      chat mode only ever sees [`ChatRepository`] +
//!      [`TurnRepository`], not the workspace switcher's surface.
//!   2. Test doubles are scoped per trait: a test that exercises a
//!      tool consent flow can stub [`PermissionGrantStore`] without
//!      touching the chat write path.
//!   3. Backend slices land independently. The migration runner +
//!      workspace repo can ship before the FTS5 wiring on chats.

use async_trait::async_trait;
use static_assertions::assert_impl_all;

use crate::ai::domain::{Conversation, Turn};
use crate::id::{AttachmentId, ChatId, PermissionGrantId, SessionId, TurnId, WorkspaceId};
use crate::persistence::error::PersistenceError;
use crate::persistence::types::{Attachment, PermissionGrant, Session, SessionMode};
use crate::workspace::{Workspace, WorkspaceRef};

// ─────────────────────────────────────────────────────────────────────
// WorkspaceRepository — workspace CRUD.
// ─────────────────────────────────────────────────────────────────────

/// Read/write surface for [`Workspace`] records.
///
/// One workspace per on-disk root; the agent loop opens a workspace
/// when the user opens a folder and closes it when the folder is
/// switched away.
///
/// Method semantics:
///
/// - [`Self::upsert`] — create-or-update. Writers do not need to know
///   whether a workspace already exists; the impl reconciles by id.
/// - [`Self::get`] — `Ok(Some(_))` when the id is known,
///   `Ok(None)` when it is not. Splitting "missing" from "broken"
///   matches the convention [`crate::ai::secret::SecretStore`]
///   establishes.
/// - [`Self::delete`] — idempotent. Deleting a missing workspace is
///   not an error — the caller's post-condition ("the id is gone")
///   already holds.
/// - [`Self::list`] — every workspace currently persisted, ordered
///   by display name. The list is small enough that pagination is
///   not in scope today; PRD-03 revisits if a power user ever
///   accumulates thousands of workspaces.
#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    /// Create or update a workspace record.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the write (IO error, optimistic-lock conflict,
    /// transient lock).
    async fn upsert(&self, workspace: &Workspace) -> Result<(), PersistenceError>;

    /// Fetch a workspace by id.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be read.
    async fn get(&self, id: WorkspaceId) -> Result<Option<Workspace>, PersistenceError>;

    /// Remove a workspace by id. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the delete.
    async fn delete(&self, id: WorkspaceId) -> Result<(), PersistenceError>;

    /// Enumerate every persisted workspace.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be enumerated.
    async fn list(&self) -> Result<Vec<Workspace>, PersistenceError>;
}

assert_impl_all!(dyn WorkspaceRepository: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// SessionRepository — sessions inside a workspace.
// ─────────────────────────────────────────────────────────────────────

/// Read/write surface for [`Session`] records.
///
/// Sessions are scoped to a workspace and a [`SessionMode`]. The
/// recents list and the workspace switcher both lean on
/// [`Self::list_for_workspace`] to render mode-bucketed recents.
#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Create or update a session record.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the write.
    async fn upsert(&self, session: &Session) -> Result<(), PersistenceError>;

    /// Fetch a session by id.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be read.
    async fn get(&self, id: SessionId) -> Result<Option<Session>, PersistenceError>;

    /// Remove a session by id. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the delete.
    async fn delete(&self, id: SessionId) -> Result<(), PersistenceError>;

    /// Enumerate every session that lives inside a workspace,
    /// optionally filtered to a single mode.
    ///
    /// Ordering: most-recently-updated first. The recents list
    /// renders this verbatim; the workspace switcher buckets by
    /// mode after.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be enumerated.
    async fn list_for_workspace(
        &self,
        workspace_id: WorkspaceId,
        mode: Option<SessionMode>,
    ) -> Result<Vec<Session>, PersistenceError>;
}

assert_impl_all!(dyn SessionRepository: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ChatRepository — chats inside a session.
// ─────────────────────────────────────────────────────────────────────

/// Read/write surface for [`Conversation`] records keyed by
/// [`ChatId`].
///
/// A chat is the conversation thread inside a [`SessionMode::Chat`]
/// session. The trait surface is intentionally narrow: chats are
/// upserted as a whole, fetched as a whole, and listed by session.
/// Turn-level append happens through [`TurnRepository`] so the FTS5
/// index can be updated transactionally next to the row.
#[async_trait]
pub trait ChatRepository: Send + Sync {
    /// Create or update a chat record.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the write.
    async fn upsert(&self, chat: &Conversation) -> Result<(), PersistenceError>;

    /// Fetch a chat by id.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be read.
    async fn get(&self, id: ChatId) -> Result<Option<Conversation>, PersistenceError>;

    /// Remove a chat by id. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the delete.
    async fn delete(&self, id: ChatId) -> Result<(), PersistenceError>;

    /// Enumerate every chat inside a session, ordered by most-recent
    /// turn first.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be enumerated.
    async fn list_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Conversation>, PersistenceError>;

    /// Full-text search inside a workspace's chats. Backed by FTS5
    /// once the impl lands; this trait surface stays a flat
    /// `query → matching chat ids` so callers do not depend on the
    /// indexer choice.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying index
    /// cannot be queried.
    async fn search(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
    ) -> Result<Vec<ChatId>, PersistenceError>;
}

assert_impl_all!(dyn ChatRepository: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// TurnRepository — turns inside a chat.
// ─────────────────────────────────────────────────────────────────────

/// Append-and-fetch surface for [`Turn`] records.
///
/// Turns are immutable once written: the agent loop appends a turn
/// at the end of an assistant pass and never edits it in place.
/// Editing a turn is modelled as a *new* turn that supersedes the
/// previous one; the impl exposes the full sequence so callers can
/// surface revision history if they want.
#[async_trait]
pub trait TurnRepository: Send + Sync {
    /// Append a turn to a chat.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the write — including [`PersistenceError::ConflictingWrite`]
    /// if a concurrent appender raced for the same `created_at`
    /// slot.
    async fn append(&self, chat_id: ChatId, turn: &Turn) -> Result<(), PersistenceError>;

    /// Fetch a turn by id.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be read.
    async fn get(&self, id: TurnId) -> Result<Option<Turn>, PersistenceError>;

    /// Enumerate every turn inside a chat, ordered by `created_at`
    /// ascending. Callers that want the most recent first reverse
    /// the iterator.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be enumerated.
    async fn list_for_chat(&self, chat_id: ChatId) -> Result<Vec<Turn>, PersistenceError>;
}

assert_impl_all!(dyn TurnRepository: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// AttachmentStore — content-addressed blob metadata + bytes.
// ─────────────────────────────────────────────────────────────────────

/// Read/write surface for [`Attachment`] metadata and the underlying
/// blob bytes.
///
/// The store is content-addressed: two attachments with identical
/// bytes share the same on-disk blob, but each carries its own
/// metadata record so provenance (which chat, which turn, which
/// filename) stays distinct. Garbage collection is handled inside
/// the impl — when the last metadata record referencing a content
/// hash is deleted, the impl reclaims the blob.
#[async_trait]
pub trait AttachmentStore: Send + Sync {
    /// Persist an attachment's metadata and its underlying bytes.
    /// The impl is free to dedupe by `content_hash`; callers do not
    /// need to check whether the bytes are already on disk.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the write.
    async fn put(&self, metadata: &Attachment, bytes: &[u8]) -> Result<(), PersistenceError>;

    /// Fetch an attachment's metadata by id.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be read.
    async fn get_metadata(&self, id: AttachmentId) -> Result<Option<Attachment>, PersistenceError>;

    /// Fetch the underlying bytes for an attachment.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::NotFound`] when the metadata
    /// record exists but the blob has been reclaimed, or any
    /// other variant when the store cannot be read.
    async fn get_bytes(&self, id: AttachmentId) -> Result<Vec<u8>, PersistenceError>;

    /// Remove an attachment by id. Reclaims the blob bytes when no
    /// metadata record references the hash any longer. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the delete.
    async fn delete(&self, id: AttachmentId) -> Result<(), PersistenceError>;

    /// Enumerate every attachment scoped to a chat. Ordered by
    /// `created_at` ascending so the chat renderer can interleave
    /// attachments with turns without a parallel sort.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be enumerated.
    async fn list_for_chat(&self, chat_id: ChatId) -> Result<Vec<Attachment>, PersistenceError>;
}

assert_impl_all!(dyn AttachmentStore: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// PermissionGrantStore — consent-surface grants per workspace.
// ─────────────────────────────────────────────────────────────────────

/// Read/write surface for [`PermissionGrant`] records.
///
/// Grants are scoped per workspace; the consent surface and the
/// agent loop both consult the store before dispatching a tool
/// call. Revocation is delete-by-id; the audit log is reconstructed
/// from `created_at` ordering.
#[async_trait]
pub trait PermissionGrantStore: Send + Sync {
    /// Persist a permission grant.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the write.
    async fn upsert(&self, grant: &PermissionGrant) -> Result<(), PersistenceError>;

    /// Fetch a grant by id.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be read.
    async fn get(&self, id: PermissionGrantId)
        -> Result<Option<PermissionGrant>, PersistenceError>;

    /// Revoke a grant by id. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the delete.
    async fn delete(&self, id: PermissionGrantId) -> Result<(), PersistenceError>;

    /// Enumerate every grant inside a workspace. Ordered by
    /// `created_at` descending so the audit log surfaces the most
    /// recent decisions first.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be enumerated.
    async fn list_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<PermissionGrant>, PersistenceError>;
}

assert_impl_all!(dyn PermissionGrantStore: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// RecentWorkspacesRepository — recents bookkeeping for the workspace
// switcher.
// ─────────────────────────────────────────────────────────────────────

/// Recents-list surface for the workspace switcher.
///
/// Distinct from [`WorkspaceRepository`] because the recents list is
/// a *view* over the workspaces — same identity, projected to a
/// lightweight handle ([`WorkspaceRef`]) and ordered by last-opened
/// time. PRD-03 keeps the storage of "when was this last opened"
/// separate from the canonical workspace record so reordering on
/// open does not have to rewrite the whole record.
#[async_trait]
pub trait RecentWorkspacesRepository: Send + Sync {
    /// Record that a workspace was just opened. The impl updates
    /// the underlying timestamp and the recents view re-sorts on
    /// the next [`Self::list`] call.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the write.
    async fn record_opened(&self, id: WorkspaceId) -> Result<(), PersistenceError>;

    /// Forget a workspace's recents entry. Used when the user
    /// removes a workspace from the recents list without deleting
    /// the underlying [`Workspace`] record. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// rejects the delete.
    async fn forget(&self, id: WorkspaceId) -> Result<(), PersistenceError>;

    /// Enumerate the recents list, most-recently-opened first.
    /// `limit` caps the result so the switcher does not have to
    /// load every historical workspace; pass `None` to fetch the
    /// full list.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] when the underlying store
    /// cannot be enumerated.
    async fn list(&self, limit: Option<usize>) -> Result<Vec<WorkspaceRef>, PersistenceError>;
}

assert_impl_all!(dyn RecentWorkspacesRepository: Send, Sync);
