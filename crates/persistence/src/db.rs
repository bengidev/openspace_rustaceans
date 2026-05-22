//! [`Database`] — async SQL handle with WAL pragma + migration on open.
//!
//! PRD-03 Slice 6 lays the foundation every later repository slice
//! borrows: a single [`tokio_rusqlite::Connection`] per process,
//! configured with the PRAGMA set the architecture (Q6) locks in, and
//! brought to the latest schema version *before* the handle is handed
//! back to the caller. Constructing a [`Database`] always leaves the
//! caller with a connection that is safe to write through.
//!
//! # PRAGMA set
//!
//! Applied once, at open time, in the order below:
//!
//! - `journal_mode = WAL` — write-ahead logging so a writer does not
//!   block readers, and so multi-window concurrent access (US#1) does
//!   not corrupt the file.
//! - `synchronous = NORMAL` — WAL-safe relaxed durability. `FULL` adds
//!   nothing on a journal that is already crash-consistent.
//! - `foreign_keys = ON` — SQLite ships with FKs disabled per
//!   connection by default; the schema relies on cascade behaviour, so
//!   the persistence handle must turn them on every time.
//! - `busy_timeout = 5000` — a writer that finds the database locked
//!   retries internally for up to five seconds before surfacing
//!   `SQLITE_BUSY`. The repository layer adds an outer retry loop on
//!   top of this in a follow-up slice; the busy_timeout removes the
//!   common-case lock contention without any caller cooperation.
//!
//! # Migration on open
//!
//! [`Database::open`] runs every numbered migration in
//! `migrations_dir` against the freshly-opened connection through
//! [`MigrationRunner`] before returning. A migration failure surfaces
//! as [`PersistenceError::MigrationFailed`] from `open` itself, which
//! the app launch path treats as a hard error per AC: "fails app
//! launch if any migration errors".
//!
//! # Concurrency model
//!
//! One [`tokio_rusqlite::Connection`] per process. Reads and writes
//! both serialise through the same handle; SQLite already serialises
//! writes file-side, and pooling reads adds complexity for marginal
//! gain at MVP scale (PRD-03 — Implementation Decisions). Cloning a
//! [`Database`] is cheap (`Arc` under the hood) so repository impls
//! that hold their own copy stay cheap to construct.
//!
//! # What this module does *not* do
//!
//! - Path resolution for the production data directory. Slice 6 takes
//!   a path; the integration slice that wires the app entry point will
//!   compute it from the `directories` crate and pass it in.
//! - Repository CRUD. Repository impls land in PRD-03 Slices 7–9, each
//!   borrowing a [`Database`] handle.
//! - Settings. [`crate::SettingsStore`] is a separate concern; the
//!   database handle does not touch `settings.toml`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openspace_shared::persistence::PersistenceError;
use rusqlite::Connection;
use tokio_rusqlite::Connection as AsyncConnection;

use crate::migration::MigrationRunner;

/// Async SQL handle the persistence crate hands to repositories.
///
/// Owns one [`tokio_rusqlite::Connection`] under an [`Arc`] so cloning
/// is cheap and every clone shares the same underlying SQLite file
/// handle. Repository impls store a [`Database`] by value and use
/// [`Database::connection`] for each query; the blocking pool inside
/// `tokio_rusqlite` is what keeps the UI thread from blocking on disk
/// IO (US#9).
#[derive(Debug, Clone)]
pub struct Database {
    /// The async wrapper. `Arc` makes clone cheap and shared.
    inner: Arc<AsyncConnection>,
    /// Resolved path to the database file. Held for diagnostics so
    /// error messages can identify which file failed without forcing
    /// the caller to thread the path through every error path.
    path: Arc<PathBuf>,
}

impl Database {
    /// Open (or create) the database at `db_path`, apply the PRAGMA
    /// set, and run every migration in `migrations_dir`.
    ///
    /// Returns a handle that is safe to hand to repository impls — the
    /// schema is at the latest version, foreign keys are on, and WAL
    /// is engaged.
    ///
    /// # Errors
    ///
    /// - [`PersistenceError::IoError`] if the database file or its
    ///   parent directory cannot be created/opened.
    /// - [`PersistenceError::MigrationFailed`] if any migration
    ///   errors. The error message identifies the failing file.
    /// - [`PersistenceError::DatabaseLocked`] if the connection cannot
    ///   acquire its first lock within the busy_timeout window.
    /// - [`PersistenceError::SchemaVersionMismatch`] if the on-disk
    ///   schema is newer than the binary knows about (downgrade
    ///   refused).
    pub async fn open(
        db_path: impl Into<PathBuf>,
        migrations_dir: impl AsRef<Path>,
    ) -> Result<Self, PersistenceError> {
        let db_path: PathBuf = db_path.into();
        let migrations_dir = migrations_dir.as_ref().to_path_buf();

        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                let parent_buf = parent.to_path_buf();
                let parent_display_join = parent_buf.display().to_string();
                let parent_display_io = parent_display_join.clone();
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(&parent_buf))
                    .await
                    .map_err(|join_err| {
                        PersistenceError::IoError(format!(
                            "creating database parent directory {parent_display_join}: {join_err}"
                        ))
                    })?
                    .map_err(|err| {
                        PersistenceError::IoError(format!(
                            "creating database parent directory {parent_display_io}: {err}"
                        ))
                    })?;
            }
        }

        let async_conn = AsyncConnection::open(&db_path)
            .await
            .map_err(map_open_error)?;

        async_conn
            .call(apply_pragmas)
            .await
            .map_err(map_call_error)?;

        let migrations_dir_for_call = migrations_dir.clone();
        async_conn
            .call(move |conn| {
                let mut runner = MigrationRunner::new(conn, &migrations_dir_for_call)
                    .map_err(persistence_to_tokio_rusqlite)?;
                runner.run().map_err(persistence_to_tokio_rusqlite)?;
                Ok(())
            })
            .await
            .map_err(map_migration_call_error)?;

        Ok(Self {
            inner: Arc::new(async_conn),
            path: Arc::new(db_path),
        })
    }

    /// Open an in-memory database for tests. The `:memory:` SQLite
    /// instance is private to the connection — every clone of the
    /// returned handle shares the same in-memory file (because the
    /// `tokio_rusqlite::Connection` is wrapped in `Arc`), but two
    /// separate calls to `open_in_memory` see different databases.
    ///
    /// The PRAGMA set is applied identically to [`Self::open`] so
    /// in-memory tests exercise the same configuration production
    /// runs under, except `journal_mode = WAL` which SQLite silently
    /// downgrades to `MEMORY` on a `:memory:` database (the WAL file
    /// would have nowhere to live).
    ///
    /// # Errors
    ///
    /// Same surface as [`Self::open`] minus the path-resolution
    /// branches.
    pub async fn open_in_memory(
        migrations_dir: impl AsRef<Path>,
    ) -> Result<Self, PersistenceError> {
        let migrations_dir = migrations_dir.as_ref().to_path_buf();

        let async_conn = AsyncConnection::open_in_memory()
            .await
            .map_err(map_open_error)?;

        async_conn
            .call(apply_pragmas)
            .await
            .map_err(map_call_error)?;

        let migrations_dir_for_call = migrations_dir.clone();
        async_conn
            .call(move |conn| {
                let mut runner = MigrationRunner::new(conn, &migrations_dir_for_call)
                    .map_err(persistence_to_tokio_rusqlite)?;
                runner.run().map_err(persistence_to_tokio_rusqlite)?;
                Ok(())
            })
            .await
            .map_err(map_migration_call_error)?;

        Ok(Self {
            inner: Arc::new(async_conn),
            path: Arc::new(PathBuf::from(":memory:")),
        })
    }

    /// Resolved path to the database file. `:memory:` for an in-memory
    /// instance.
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Borrow the async connection for repository call sites.
    ///
    /// Repository impls reach for this to dispatch read or write
    /// queries onto the blocking pool:
    ///
    /// ```text
    /// db.connection().call(|conn| { /* rusqlite calls */ }).await?;
    /// ```
    pub fn connection(&self) -> &AsyncConnection {
        &self.inner
    }
}

/// Apply the PRAGMA set documented in the module-level docs.
///
/// Pulled out of [`Database::open`] so the same routine runs on the
/// in-memory test path. Returns a `tokio_rusqlite::Result` because
/// the closure runs inside `Connection::call`; the outer `open`
/// translates that into [`PersistenceError`].
fn apply_pragmas(conn: &mut Connection) -> tokio_rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(tokio_rusqlite::Error::from)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(tokio_rusqlite::Error::from)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(tokio_rusqlite::Error::from)?;
    conn.pragma_update(None, "busy_timeout", 5000_i64)
        .map_err(tokio_rusqlite::Error::from)?;
    Ok(())
}

/// Map a `tokio_rusqlite::Error` raised by `open` itself to the
/// appropriate [`PersistenceError`] variant. The IO branches surface
/// as `IoError`; lock contention surfaces as `DatabaseLocked`; every
/// other SQLite failure surfaces as `MigrationFailed` since `open`
/// has no other failure category in its public surface.
fn map_open_error(err: tokio_rusqlite::Error) -> PersistenceError {
    match &err {
        tokio_rusqlite::Error::Rusqlite(rusqlite::Error::SqliteFailure(code, _)) => {
            match code.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    PersistenceError::DatabaseLocked(format!("opening database: {err}"))
                }
                _ => PersistenceError::IoError(format!("opening database: {err}")),
            }
        }
        _ => PersistenceError::IoError(format!("opening database: {err}")),
    }
}

/// Map a `tokio_rusqlite::Error` from a generic `call` (PRAGMA setup
/// in particular) into [`PersistenceError`]. Lock contention during
/// PRAGMA application is unusual but possible if another process is
/// migrating the same file; surface that distinctly so the operator
/// can tell apart "PRAGMA failed" from "another window is mid-migrate".
/// Anything else is a configuration / IO failure: PRAGMA application
/// is *not* a migration step, so funnelling it through `MigrationFailed`
/// would mislead callers that match on that variant to decide whether
/// to wipe and retry.
fn map_call_error(err: tokio_rusqlite::Error) -> PersistenceError {
    match &err {
        tokio_rusqlite::Error::Rusqlite(rusqlite::Error::SqliteFailure(code, _)) => {
            match code.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    PersistenceError::DatabaseLocked(format!("configuring database: {err}"))
                }
                _ => PersistenceError::IoError(format!("configuring database: {err}")),
            }
        }
        _ => PersistenceError::IoError(format!("configuring database: {err}")),
    }
}

/// Map a `tokio_rusqlite::Error` from the migration `call` into
/// [`PersistenceError`]. The inner closure already returns a
/// [`PersistenceError`] re-wrapped as `tokio_rusqlite::Error::Other`;
/// this helper unwraps that round-trip so the caller sees the original
/// error category (MigrationFailed vs SchemaVersionMismatch vs
/// IoError) rather than a flattened `MigrationFailed`.
fn map_migration_call_error(err: tokio_rusqlite::Error) -> PersistenceError {
    if let tokio_rusqlite::Error::Other(boxed) = &err {
        if let Some(inner) = boxed.downcast_ref::<PersistenceError>() {
            return inner.clone();
        }
    }
    PersistenceError::MigrationFailed(format!("running migrations: {err}"))
}

/// Wrap a [`PersistenceError`] so it can travel through
/// `tokio_rusqlite::Result`. The outer `map_migration_call_error`
/// unwraps it on the other side.
fn persistence_to_tokio_rusqlite(err: PersistenceError) -> tokio_rusqlite::Error {
    tokio_rusqlite::Error::Other(Box::new(err))
}

#[cfg(test)]
mod tests {
    //! Slice 6 acceptance tests.
    //!
    //! Every scenario uses an isolated tempdir for the migrations and
    //! either a `:memory:` or tempdir-backed database file. No test
    //! touches a global path or shares state with another.

    use super::*;
    use tempfile::TempDir;

    /// Drop the production `0001_initial.sql` next to a tempdir so
    /// the migration runner picks it up. Returns the tempdir handle
    /// (its `Drop` is what cleans up — keep it alive for the whole
    /// test).
    fn fixture_migrations_with_initial() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let initial = include_str!("../migrations/0001_initial.sql");
        std::fs::write(dir.path().join("0001_initial.sql"), initial).expect("write 0001 fixture");
        dir
    }

    #[tokio::test]
    async fn open_in_memory_applies_pragmas_and_runs_migrations() {
        let migrations = fixture_migrations_with_initial();
        let db = Database::open_in_memory(migrations.path())
            .await
            .expect("open in-memory");

        // Foreign keys are on (per-connection PRAGMA — easy to forget
        // in the wiring; explicit assertion guards against regression).
        let fk: i64 = db
            .connection()
            .call(|conn| {
                conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))
                    .map_err(tokio_rusqlite::Error::from)
            })
            .await
            .expect("read foreign_keys pragma");
        assert_eq!(fk, 1);

        let busy: i64 = db
            .connection()
            .call(|conn| {
                conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                    .map_err(tokio_rusqlite::Error::from)
            })
            .await
            .expect("read busy_timeout pragma");
        assert_eq!(busy, 5000);

        // `synchronous = NORMAL` (= 1) is what makes WAL-mode commits
        // cheap; `FULL` (= 2) would silently regress write throughput
        // without changing crash-consistency on a journal that is
        // already crash-consistent.
        let sync_mode: i64 = db
            .connection()
            .call(|conn| {
                conn.query_row("PRAGMA synchronous", [], |row| row.get(0))
                    .map_err(tokio_rusqlite::Error::from)
            })
            .await
            .expect("read synchronous pragma");
        assert_eq!(sync_mode, 1);

        // `journal_mode` on a `:memory:` database is silently downgraded
        // by SQLite to `MEMORY` because the WAL sidecar files have
        // nowhere to live; assert the downgrade explicitly so a
        // regression that broke the PRAGMA call entirely (returning the
        // default `memory` because nothing was applied) is still caught
        // by `open_on_disk_round_trips_through_a_real_file`, which
        // exercises the on-disk path where WAL *is* observed.
        let journal_mode: String = db
            .connection()
            .call(|conn| {
                conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
                    .map_err(tokio_rusqlite::Error::from)
            })
            .await
            .expect("read journal_mode pragma");
        assert_eq!(journal_mode.to_lowercase(), "memory");

        // Schema is at the highest known version (0001 only, for now).
        let version: i64 = db
            .connection()
            .call(|conn| {
                conn.query_row("SELECT schema_version FROM _meta LIMIT 1", [], |row| {
                    row.get(0)
                })
                .map_err(tokio_rusqlite::Error::from)
            })
            .await
            .expect("read schema version");
        assert_eq!(version, 1);
    }

    #[tokio::test]
    async fn open_in_memory_creates_every_initial_table() {
        let migrations = fixture_migrations_with_initial();
        let db = Database::open_in_memory(migrations.path())
            .await
            .expect("open in-memory");

        let names: Vec<String> = db
            .connection()
            .call(|conn| {
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("list tables");

        for required in [
            "_meta",
            "attachments",
            "chats",
            "permission_grants",
            "recent_workspaces",
            "sessions",
            "turns",
            "workspaces",
        ] {
            assert!(
                names.iter().any(|n| n == required),
                "missing table {required}, found {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn open_on_disk_round_trips_through_a_real_file() {
        let migrations = fixture_migrations_with_initial();
        let dir = TempDir::new().expect("tempdir for db");
        let db_path = dir.path().join("data.db");

        let db = Database::open(&db_path, migrations.path())
            .await
            .expect("open disk db");
        assert_eq!(db.path(), db_path.as_path());

        // On a real file WAL actually engages — assert it so a
        // regression that drops the `journal_mode` PRAGMA call is
        // caught here (the in-memory test cannot observe WAL because
        // SQLite downgrades it to MEMORY for `:memory:` databases).
        let journal_mode: String = db
            .connection()
            .call(|conn| {
                conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
                    .map_err(tokio_rusqlite::Error::from)
            })
            .await
            .expect("read journal_mode pragma");
        assert_eq!(journal_mode.to_lowercase(), "wal");

        // Re-opening the same file is idempotent — the migration
        // runner sees a fully-migrated database and short-circuits.
        let db2 = Database::open(&db_path, migrations.path())
            .await
            .expect("re-open disk db");
        let version: i64 = db2
            .connection()
            .call(|conn| {
                conn.query_row("SELECT schema_version FROM _meta LIMIT 1", [], |row| {
                    row.get(0)
                })
                .map_err(tokio_rusqlite::Error::from)
            })
            .await
            .expect("read schema version after re-open");
        assert_eq!(version, 1);
    }

    #[tokio::test]
    async fn missing_migrations_directory_surfaces_io_error() {
        let bogus = std::path::PathBuf::from("/this/path/does/not/exist/migrations");
        let result = Database::open_in_memory(&bogus).await;
        match result {
            Err(PersistenceError::IoError(_)) => {}
            other => panic!("expected IoError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn migration_failure_surfaces_migration_failed() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("0001_broken.sql"), "NOT VALID SQL AT ALL;")
            .expect("write broken migration");

        let result = Database::open_in_memory(dir.path()).await;
        match result {
            Err(PersistenceError::MigrationFailed(msg)) => {
                assert!(
                    msg.contains("0001_broken.sql"),
                    "error must identify the failing file, got: {msg}"
                );
            }
            other => panic!("expected MigrationFailed, got {other:?}"),
        }
    }
}
