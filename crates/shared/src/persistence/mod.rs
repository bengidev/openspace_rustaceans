//! Persistence trait surface.
//!
//! Domain-side declarations the persistence layer (PRD-03) implements.
//! Every repository or store the rest of the PRD ships plugs in through
//! a trait declared here, so the agent loop, feature crates, and the
//! settings pipeline talk to a stable shape regardless of which
//! backend currently provides the impl.
//!
//! # Module map
//!
//! - [`error`] — [`PersistenceError`], the failure vocabulary every
//!   repository surfaces. Carries the seven categories PRD-03
//!   distinguishes (`MigrationFailed`, `IoError`,
//!   `SerializationError`, `NotFound`, `ConflictingWrite`,
//!   `SchemaVersionMismatch`, `DatabaseLocked`).
//! - [`types`] — supporting Domain types the repository surface
//!   references but that do not yet have a home in the rest of the
//!   Domain crate (`Session`, `Attachment`, `PermissionGrant` and
//!   their enums).
//! - [`repository`] — the seven repository / store traits PRD-03 will
//!   implement: [`WorkspaceRepository`], [`SessionRepository`],
//!   [`ChatRepository`], [`TurnRepository`], [`AttachmentStore`],
//!   [`PermissionGrantStore`], [`RecentWorkspacesRepository`].
//!
//! # Hard rules
//!
//! - Trait method bodies are signature-only in this slice. Concrete
//!   impls live on `openspace-persistence` and land in follow-up
//!   PRD-03 slices.
//! - Every public trait and supporting type is `Send + Sync`,
//!   asserted at compile time via `static_assertions::assert_impl_all`.
//!   Object-safe traits additionally pin
//!   `dyn Trait: Send + Sync` so a future change cannot quietly break
//!   the sharing pattern the agent loop relies on.
//! - All identifiers, error strings, and doc comments speak the
//!   `CONTEXT.md` vocabulary (workspace, session, chat, turn,
//!   attachment, permission grant). No third-party product names
//!   appear in the surface.
//!
//! # Why a foundation slice ships an empty trait surface
//!
//! Two reasons:
//!
//!   1. Downstream feature crates (chat, recents, settings) start
//!      writing against the trait shape immediately, even before the
//!      SQL backend lands. They take `&dyn WorkspaceRepository` and
//!      friends as constructor arguments and stay testable through
//!      the same trait surface a future test double will satisfy.
//!   2. The PRD-03 issues that follow this one each land *one*
//!      backend slice — migration runner, workspace + session repo,
//!      chat + turn repo, FTS5 wiring, attachment store, permission
//!      store, settings pipeline — without each touching the trait
//!      surface itself. The trait shape is the contract; backend
//!      slices are the implementations.

pub mod error;
pub mod repository;
pub mod types;

pub use error::PersistenceError;
pub use repository::{
    AttachmentStore, ChatRepository, PermissionGrantStore, RecentWorkspacesRepository,
    SessionRepository, TurnRepository, WorkspaceRepository,
};
pub use types::{
    Attachment, PermissionDecision, PermissionGrant, PermissionScope, Session, SessionMode,
};
