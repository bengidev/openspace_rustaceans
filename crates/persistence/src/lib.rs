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
//! None of those modules exist yet. They land as additive slices so a
//! reviewer can read each one in isolation.

#![forbid(unsafe_code)]
