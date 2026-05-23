//! On-disk persistence for OpenSpace.
//!
//! This crate is the home for the SQL-backed repository implementations
//! and the settings-file pipeline that PRD-03 introduces. The
//! foundation slice (issue #63) ships the crate skeleton only — every
//! repository trait this crate will implement lives on
//! [`openspace_shared::persistence`], and the impls land in follow-up
//! slices.
//!
//! # Why this crate exists today, before any code
//!
//! Splitting infrastructure off `openspace-shared` early keeps two
//! invariants stable as the persistence layer grows:
//!
//!   1. The Domain crate stays free of storage dependencies — it can
//!      compile and test on a host that has no database, no migration
//!      runner, no settings file. The repository *trait surface* is
//!      Domain; the *impls* are infrastructure and live here.
//!   2. Downstream feature crates that depend on a persisted view
//!      (chat history, recent workspaces, attachment blobs) reach for
//!      a single concrete crate name from day one rather than
//!      following a moving target.
//!
//! # Module layout (forecast)
//!
//! Later PRD-03 slices populate this crate roughly along the lines of:
//!
//! - `migration` — embedded SQL migrations, schema-version checks.
//! - `workspace` — `WorkspaceRepository` impl over the SQL backend.
//! - `session` — `SessionRepository` impl, including ordering and
//!   recents bookkeeping.
//! - `chat` — `ChatRepository` + `TurnRepository` impls and the FTS5
//!   index wiring.
//! - `attachment` — content-addressed blob store on disk.
//! - `permission` — `PermissionGrantStore` impl scoped per workspace.
//! - `settings` — settings-TOML serializer/deserializer.
//!
//! Modules that have landed so far:
//!
//! - [`settings_store`] — PRD-03 Slice 3 (issue #65). In-process owner
//!   of `settings.toml` with a temp-then-rename atomic write pipeline
//!   on the tokio blocking pool. Hot reload lands in a follow-up
//!   slice.
//! - [`migration`] — PRD-03 Slice 5 (issue #67). [`MigrationRunner`]
//!   owns the schema-version state machine over a numbered SQL
//!   migrations directory. The runner is the deep module every
//!   later repository slice opens its connection through; the SQL
//!   schema itself lands in follow-up slices, not here.
//! - [`db`] — PRD-03 Slice 6 (issue #68). [`Database`] is the async
//!   SQL handle every later repository slice borrows: opens the file
//!   with the WAL pragma set, runs every migration in
//!   `migrations/` (starting with `0001_initial.sql`, also added by
//!   this slice), and hands back a clone-cheap handle keyed off
//!   `tokio_rusqlite::Connection`.
//!
//! Everything else above is still on the forecast — additive slices so
//! a reviewer can read each one in isolation.

#![forbid(unsafe_code)]

pub mod chat;
pub mod db;
pub mod migration;
pub mod session;
pub mod settings_store;
mod sql;
pub mod turn;
pub mod workspace;

pub use chat::SqliteChatRepository;
pub use db::Database;
pub use migration::MigrationRunner;
pub use session::SqliteSessionRepository;
pub use settings_store::SettingsStore;
pub use turn::SqliteTurnRepository;
pub use workspace::{SqliteRecentWorkspacesRepository, SqliteWorkspaceRepository};
