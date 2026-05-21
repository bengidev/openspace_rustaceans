//! Workspace identity types.
//!
//! A *workspace* is the on-disk root the user is working in: the
//! folder that contains chats, buffers, terminal sessions, and the
//! project the home shell points at. Everything persistable is keyed
//! by [`WorkspaceId`].
//!
//! This slice lands two narrow shapes:
//!
//! - [`Workspace`] — the full record. Carries the id, the on-disk
//!   path, and a human-friendly display name. Concrete enough to
//!   compile, narrow enough that PRD-03 (persistence) can grow it
//!   with optional fields (`last_opened_at`, `pinned_at`, …)
//!   without a breaking change.
//! - [`WorkspaceRef`] — the lightweight handle the recents list and
//!   the workspace switcher pass around. Just an id + display name,
//!   so a UI surface that only renders names never has to load the
//!   full record from disk.
//!
//! Both types are `Send + Sync` (compile-time enforced) and serde
//! round-trippable. The serde shape is locked so storage written by
//! today's binary keeps loading after later PRDs extend the struct.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

use crate::id::WorkspaceId;

/// On-disk workspace identity.
///
/// Three fields, no behaviour. Persistence (PRD-03) owns the question
/// of *where* this lives and *how* it is loaded; the Domain layer
/// only owns the shape.
///
/// `#[non_exhaustive]` so adding fields like `last_opened_at` or
/// `pinned_at` in later PRDs is a non-breaking change for downstream
/// callers — they construct via the explicit constructor below
/// instead of struct literals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Workspace {
    /// Stable identity. Survives renames and moves.
    pub id: WorkspaceId,
    /// Filesystem root of the workspace.
    pub root: PathBuf,
    /// Human-friendly label rendered in the workspace switcher and
    /// recents list. Not required to match the directory name —
    /// users rename workspaces independently of the folder.
    pub display_name: String,
}

impl Workspace {
    /// Construct a `Workspace` from its three core fields.
    ///
    /// Wrapping struct construction in a method (rather than relying
    /// on struct literals) is what makes the `#[non_exhaustive]`
    /// guarantee meaningful: callers cannot accidentally depend on
    /// the field set, so adding a new optional field later is
    /// non-breaking.
    #[must_use]
    pub fn new(id: WorkspaceId, root: PathBuf, display_name: String) -> Self {
        Self {
            id,
            root,
            display_name,
        }
    }

    /// Lightweight handle suitable for UI lists. Mirrors `display_name`
    /// and the id so renderers do not have to keep the full record
    /// around.
    #[must_use]
    pub fn as_ref(&self) -> WorkspaceRef {
        WorkspaceRef {
            id: self.id,
            display_name: self.display_name.clone(),
        }
    }
}

/// Lightweight workspace handle.
///
/// Used by the recents list, the workspace switcher, and any other UI
/// surface that renders workspaces without needing the full on-disk
/// path. Cheap to clone and serialise.
///
/// `#[non_exhaustive]` for the same reason as [`Workspace`]: future
/// optional fields stay non-breaking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceRef {
    /// Stable identity matching the underlying workspace.
    pub id: WorkspaceId,
    /// Display name as cached by the surface that produced this ref.
    /// May lag behind the canonical [`Workspace::display_name`] if
    /// the user renamed the workspace and the cache has not yet been
    /// refreshed.
    pub display_name: String,
}

impl WorkspaceRef {
    /// Build a ref from its parts. Same `#[non_exhaustive]` rationale
    /// as [`Workspace::new`].
    #[must_use]
    pub fn new(id: WorkspaceId, display_name: String) -> Self {
        Self { id, display_name }
    }
}

assert_impl_all!(Workspace: Send, Sync);
assert_impl_all!(WorkspaceRef: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_workspace() -> Workspace {
        Workspace::new(
            WorkspaceId::new_v4(),
            PathBuf::from("/tmp/workspaces/example"),
            "Example".to_string(),
        )
    }

    /// JSON round-trip locks the wire format. Persistence (PRD-03)
    /// will store these as JSON blobs; a silent rename to a different
    /// shape would break every existing snapshot.
    #[test]
    fn workspace_round_trips_through_json() {
        let original = sample_workspace();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Workspace = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn workspace_ref_round_trips_through_json() {
        let original = WorkspaceRef::new(WorkspaceId::new_v4(), "Recents entry".to_string());
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: WorkspaceRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// `as_ref` projects the same identity and label so a UI that
    /// only ever holds the ref cannot drift from the source record
    /// at construction time.
    #[test]
    fn as_ref_mirrors_id_and_display_name() {
        let workspace = sample_workspace();
        let lightweight = workspace.as_ref();
        assert_eq!(lightweight.id, workspace.id);
        assert_eq!(lightweight.display_name, workspace.display_name);
    }

    /// Lock the wire field names. Persistence depends on the JSON
    /// keys staying exactly `id`, `root`, `display_name` so a Rust
    /// rename refactor does not silently break stored data.
    #[test]
    fn wire_field_names_are_stable() {
        let workspace = Workspace::new(
            WorkspaceId::from_uuid(uuid::Uuid::nil()),
            PathBuf::from("/root"),
            "Name".to_string(),
        );
        let value: serde_json::Value =
            serde_json::to_value(&workspace).expect("serialize as value");
        let object = value.as_object().expect("object");
        assert!(object.contains_key("id"));
        assert!(object.contains_key("root"));
        assert!(object.contains_key("display_name"));
        assert_eq!(object.len(), 3, "no unexpected fields");
    }
}
