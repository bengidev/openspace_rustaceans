//! [`MigrationRunner`] — schema-version owner for the SQL backend.
//!
//! The runner is the single entry point that brings a SQLite database
//! from whatever version it currently sits at to the highest version
//! the running binary knows about. Every other persistence module in
//! the workspace assumes the schema is at the expected version before
//! it touches a connection — the runner is what makes that assumption
//! safe.
//!
//! # Migration files
//!
//! The runner reads numbered SQL files from a single directory:
//!
//! ```text
//! migrations/
//!   0001_initial_schema.sql
//!   0002_add_attachment_blobs.sql
//!   0003_chat_fts5_index.sql
//! ```
//!
//! - The number prefix (the substring before the first `_`) is parsed
//!   as a positive integer; the rest of the filename is human prose
//!   and never touched by the runner.
//! - Numbers must be unique. A duplicated version surfaces as
//!   [`PersistenceError::MigrationFailed`] *before* any SQL runs, so
//!   the on-disk schema cannot be moved forward by an ambiguous set
//!   of files.
//! - Numbers do not need to be contiguous; gaps are allowed but
//!   discouraged. The runner only cares that they sort.
//!
//! Migration files are append-only by convention. Once a number has
//! shipped, its file is frozen — corrections land as a new, higher
//! number that walks the schema forward. Editing a shipped migration
//! breaks every database that already applied it, with no path back.
//!
//! # Run-time behaviour
//!
//! [`MigrationRunner::run`] executes the following sequence:
//!
//! 1. Ensure the `_meta` table exists and carries exactly one row
//!    with `schema_version` defaulting to `0`.
//! 2. Read the current `schema_version` and the highest version on
//!    disk.
//! 3. If the current version exceeds the highest known migration,
//!    return [`PersistenceError::SchemaVersionMismatch`] — the
//!    "downgrade not supported" path. The runner never deletes,
//!    rewrites, or rolls back schema state without a fresh, higher
//!    migration explicitly doing so.
//! 4. Otherwise, apply every migration whose number exceeds the
//!    current version, in ascending order, each inside its own
//!    [`rusqlite::Transaction`]. The `_meta` row is updated inside
//!    the same transaction so a crash between SQL execution and
//!    version bookkeeping is impossible.
//! 5. A failing migration leaves the transaction unbcommitted; the
//!    transaction drops on the error path, SQLite rolls back, and
//!    the database is left exactly at the version it occupied
//!    before the failed migration began. The error surfaces as
//!    [`PersistenceError::MigrationFailed`] carrying the file name
//!    that failed and the underlying SQLite diagnostic.
//!
//! Re-running [`MigrationRunner::run`] on a fully-migrated database
//! is a no-op — the loop in step 4 finds nothing above the current
//! version and returns immediately.
//!
//! # Error mapping
//!
//! - SQL execution failures during `CREATE`, `INSERT`, `UPDATE` →
//!   [`PersistenceError::MigrationFailed`].
//! - `SQLITE_BUSY` / `SQLITE_LOCKED` while opening a transaction →
//!   [`PersistenceError::DatabaseLocked`].
//! - File IO problems while listing or reading the migrations
//!   directory → [`PersistenceError::IoError`].
//! - Unparseable filenames or duplicate version numbers →
//!   [`PersistenceError::MigrationFailed`] (the operator-facing
//!   message identifies the offending file).
//! - Version on disk above the highest known →
//!   [`PersistenceError::SchemaVersionMismatch`].

use std::fs;
use std::path::{Path, PathBuf};

use openspace_shared::persistence::PersistenceError;
use rusqlite::{Connection, ErrorCode};

/// Name of the metadata table the runner manages. The leading
/// underscore visually separates it from application tables and
/// signals "do not touch by hand". Only the runner reads or writes
/// this table.
const META_TABLE: &str = "_meta";

/// Parsed migration file ready to apply.
///
/// Held in memory for the lifetime of a [`MigrationRunner`] so the
/// run loop never re-reads the directory mid-flight; a migration
/// directory mutation between two `run()` calls is fine, but a
/// mutation between two iterations of the same call is not.
#[derive(Debug, Clone)]
struct Migration {
    /// Numeric prefix, parsed from the filename. The runner uses
    /// this both to sort and to set `schema_version` after a
    /// successful apply.
    version: i64,
    /// Original file name (e.g. `0002_add_attachment_blobs.sql`).
    /// Carried verbatim so [`PersistenceError::MigrationFailed`]
    /// surfaces an operator-friendly identifier on failure.
    file_name: String,
    /// SQL body, read from disk at discovery time. SQLite is
    /// happy to execute multiple statements separated by `;` via
    /// [`rusqlite::Connection::execute_batch`], so the file may
    /// contain `CREATE TABLE`, `CREATE INDEX`, etc. in sequence.
    sql: String,
}

/// Schema-version owner over a [`rusqlite::Connection`].
///
/// Construct one with [`MigrationRunner::new`]; call
/// [`MigrationRunner::run`] to bring the database up to the highest
/// version the migrations directory describes. The runner borrows
/// the connection mutably for the duration of the call so it can
/// open a transaction per migration without fighting the borrow
/// checker.
#[derive(Debug)]
pub struct MigrationRunner<'a> {
    /// SQL connection the runner mutates. Borrowed mutably so the
    /// runner can open transactions; the caller keeps ownership of
    /// the underlying [`Connection`] and reuses it for repository
    /// work after [`Self::run`] returns.
    conn: &'a mut Connection,
    /// In-memory list of migrations discovered at construction
    /// time, sorted ascending by [`Migration::version`].
    migrations: Vec<Migration>,
}

impl<'a> MigrationRunner<'a> {
    /// Build a runner against `conn`, discovering numbered SQL files
    /// from `migrations_dir`.
    ///
    /// Discovery happens up front so a malformed migrations directory
    /// (duplicate version, unparseable filename, missing directory)
    /// fails loudly *before* any SQL runs against the database. That
    /// keeps the failure mode "we did not start" rather than "we
    /// started and got halfway".
    ///
    /// # Errors
    ///
    /// - [`PersistenceError::IoError`] if `migrations_dir` cannot be
    ///   read.
    /// - [`PersistenceError::MigrationFailed`] if a filename does not
    ///   match the `<number>_<rest>.sql` shape, or if two files share
    ///   the same numeric prefix.
    pub fn new(
        conn: &'a mut Connection,
        migrations_dir: impl AsRef<Path>,
    ) -> Result<Self, PersistenceError> {
        let migrations = discover_migrations(migrations_dir.as_ref())?;
        Ok(Self { conn, migrations })
    }

    /// Bring the database up to the highest known migration version.
    ///
    /// Idempotent: a fully-migrated database short-circuits without
    /// touching schema. See the module docs for the full state
    /// machine and error mapping.
    pub fn run(&mut self) -> Result<(), PersistenceError> {
        ensure_meta_table(self.conn)?;
        let current = read_schema_version(self.conn)?;
        let highest_known = self.migrations.last().map(|m| m.version).unwrap_or(0);

        if current > highest_known {
            return Err(PersistenceError::SchemaVersionMismatch(format!(
                "database at version {current}, highest known migration is {highest_known}"
            )));
        }

        for migration in self.migrations.iter().filter(|m| m.version > current) {
            apply_migration(self.conn, migration)?;
        }

        Ok(())
    }
}

/// Read the migrations directory into a sorted, validated vector.
///
/// Pulled out of [`MigrationRunner::new`] so the discovery step is
/// independently testable and so the runner's `new` stays readable.
fn discover_migrations(dir: &Path) -> Result<Vec<Migration>, PersistenceError> {
    let entries = fs::read_dir(dir).map_err(|err| {
        PersistenceError::IoError(format!(
            "reading migrations directory {}: {err}",
            dir.display()
        ))
    })?;

    let mut migrations: Vec<Migration> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            PersistenceError::IoError(format!(
                "iterating migrations directory {}: {err}",
                dir.display()
            ))
        })?;
        let path: PathBuf = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("sql") {
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                PersistenceError::MigrationFailed(format!(
                    "migration filename is not valid utf-8: {}",
                    path.display()
                ))
            })?
            .to_string();

        let version = parse_version_prefix(&file_name)?;

        let sql = fs::read_to_string(&path).map_err(|err| {
            PersistenceError::IoError(format!("reading migration {}: {err}", path.display()))
        })?;

        migrations.push(Migration {
            version,
            file_name,
            sql,
        });
    }

    migrations.sort_by_key(|m| m.version);

    // Reject duplicate version numbers. A repeated prefix means two
    // files claim the same schema slot — the runner cannot pick a
    // winner without a heuristic the operator did not opt into, so
    // we refuse to start instead of guessing.
    for window in migrations.windows(2) {
        if window[0].version == window[1].version {
            return Err(PersistenceError::MigrationFailed(format!(
                "duplicate migration version {}: {} and {}",
                window[0].version, window[0].file_name, window[1].file_name
            )));
        }
    }

    Ok(migrations)
}

/// Pull the numeric prefix off a `NNNN_<name>.sql` filename.
fn parse_version_prefix(file_name: &str) -> Result<i64, PersistenceError> {
    let prefix = file_name.split('_').next().unwrap_or("");
    if prefix.is_empty() {
        return Err(PersistenceError::MigrationFailed(format!(
            "migration filename has no version prefix: {file_name}"
        )));
    }
    prefix.parse::<i64>().map_err(|err| {
        PersistenceError::MigrationFailed(format!(
            "migration filename {file_name} has unparseable version prefix {prefix:?}: {err}"
        ))
    })
}

/// Make sure the `_meta` table exists and carries exactly one row.
///
/// Runs outside the per-migration transaction so the bookkeeping
/// table can be created on a brand-new database that has nothing
/// else in it. The `INSERT` is guarded by a count check rather than
/// `INSERT OR IGNORE` so a corrupted `_meta` (more than one row) is
/// surfaced as an explicit error instead of silently picking one.
fn ensure_meta_table(conn: &mut Connection) -> Result<(), PersistenceError> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {META_TABLE} (schema_version INTEGER NOT NULL);"
    ))
    .map_err(|err| map_setup_error("creating _meta table", err))?;

    let count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {META_TABLE}"), [], |row| {
            row.get(0)
        })
        .map_err(|err| map_setup_error("reading _meta row count", err))?;

    match count {
        0 => {
            conn.execute(
                &format!("INSERT INTO {META_TABLE} (schema_version) VALUES (0)"),
                [],
            )
            .map_err(|err| map_setup_error("seeding _meta row", err))?;
            Ok(())
        }
        1 => Ok(()),
        n => Err(PersistenceError::MigrationFailed(format!(
            "_meta table is corrupt: expected 0 or 1 rows, found {n}"
        ))),
    }
}

/// Read the current `schema_version`, defaulting to `0` when the
/// `_meta` row has not yet been written. Callers must invoke
/// [`ensure_meta_table`] before this function — the default branch
/// is defensive only.
fn read_schema_version(conn: &Connection) -> Result<i64, PersistenceError> {
    conn.query_row(
        &format!("SELECT schema_version FROM {META_TABLE} LIMIT 1"),
        [],
        |row| row.get::<_, i64>(0),
    )
    .map_err(|err| map_setup_error("reading schema_version", err))
}

/// Apply a single migration inside a fresh transaction.
///
/// Both the migration body and the `_meta` update sit inside the
/// same transaction. A failure on either statement drops the
/// transaction without committing, which surfaces as a SQLite
/// rollback — the database is left exactly at the version it
/// occupied before the call.
fn apply_migration(conn: &mut Connection, migration: &Migration) -> Result<(), PersistenceError> {
    let tx = conn
        .transaction()
        .map_err(|err| map_setup_error("opening migration transaction", err))?;

    tx.execute_batch(&migration.sql).map_err(|err| {
        PersistenceError::MigrationFailed(format!("{}: {err}", migration.file_name))
    })?;

    tx.execute(
        &format!("UPDATE {META_TABLE} SET schema_version = ?1"),
        [migration.version],
    )
    .map_err(|err| {
        PersistenceError::MigrationFailed(format!(
            "{}: updating schema_version: {err}",
            migration.file_name
        ))
    })?;

    tx.commit().map_err(|err| {
        PersistenceError::MigrationFailed(format!(
            "{}: committing migration: {err}",
            migration.file_name
        ))
    })?;

    Ok(())
}

/// Map a `rusqlite` error from a runner-internal helper (table
/// setup, version read) to the appropriate
/// [`PersistenceError`] category. The migration body itself
/// always goes through [`PersistenceError::MigrationFailed`]; this
/// helper only covers the runner's own bookkeeping calls.
fn map_setup_error(context: &str, err: rusqlite::Error) -> PersistenceError {
    if let rusqlite::Error::SqliteFailure(code, _) = &err {
        match code.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                return PersistenceError::DatabaseLocked(format!("{context}: {err}"));
            }
            _ => {}
        }
    }
    PersistenceError::MigrationFailed(format!("{context}: {err}"))
}

#[cfg(test)]
mod tests {
    //! Acceptance-criteria coverage for [`MigrationRunner`].
    //!
    //! Every test wires a fresh in-memory SQLite connection plus an
    //! isolated [`tempfile::TempDir`] holding inline fixture
    //! migrations. No real schema files exist yet — the runner is a
    //! mechanism slice, so the fixtures here are deliberately
    //! synthetic (`fixtures` table, `extra` table) rather than
    //! anything PRD-03 will eventually ship.

    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Convenience: write `contents` to `dir/<name>` and return the
    /// path. Tests use this to drop fixture migrations into a
    /// scratch directory at the start of every scenario.
    fn write_migration(dir: &TempDir, name: &str, contents: &str) {
        std::fs::write(dir.path().join(name), contents).expect("write fixture migration");
    }

    /// Convenience: open a fresh in-memory database. Each call
    /// returns a new, empty database — they share nothing.
    fn fresh_db() -> Connection {
        Connection::open_in_memory().expect("open in-memory sqlite")
    }

    /// Read the current `schema_version` from `_meta`. Used by the
    /// scenarios to assert that the runner moved the version to
    /// the expected number.
    fn read_version(conn: &Connection) -> i64 {
        conn.query_row("SELECT schema_version FROM _meta LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("read schema_version")
    }

    #[test]
    fn fresh_db_with_two_fixtures_lands_at_version_two() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "0001_initial.sql",
            "CREATE TABLE fixtures (id INTEGER PRIMARY KEY);",
        );
        write_migration(
            &dir,
            "0002_extra.sql",
            "CREATE TABLE extra (id INTEGER PRIMARY KEY);",
        );

        let mut conn = fresh_db();
        let mut runner = MigrationRunner::new(&mut conn, dir.path()).expect("construct runner");
        runner.run().expect("run migrations");

        assert_eq!(read_version(&conn), 2);

        // Both tables actually exist on disk.
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare");
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();
        assert!(names.contains(&"fixtures".to_string()));
        assert!(names.contains(&"extra".to_string()));
        assert!(names.contains(&"_meta".to_string()));
    }

    #[test]
    fn rerun_on_fully_migrated_db_is_a_no_op() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "0001_initial.sql",
            "CREATE TABLE fixtures (id INTEGER PRIMARY KEY);",
        );

        let mut conn = fresh_db();
        MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner")
            .run()
            .expect("first run");

        // Second run should not error and should not change the
        // version. We also assert that `fixtures` was not recreated
        // by checking the runner did not re-apply the SQL — the
        // table being there from the first run is enough to confirm
        // the no-op semantics, but a second `CREATE TABLE` would
        // fail without `IF NOT EXISTS`, so a clean second run is
        // the strongest signal we have.
        MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner second time")
            .run()
            .expect("second run is idempotent");

        assert_eq!(read_version(&conn), 1);
    }

    #[test]
    fn partially_migrated_db_advances_to_highest_version() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "0001_initial.sql",
            "CREATE TABLE fixtures (id INTEGER PRIMARY KEY);",
        );

        let mut conn = fresh_db();
        // First pass: only 0001 exists, so the DB lands at version 1.
        MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner")
            .run()
            .expect("apply 0001");
        assert_eq!(read_version(&conn), 1);

        // Now drop 0002 in and re-run; only 0002 should apply.
        write_migration(
            &dir,
            "0002_extra.sql",
            "CREATE TABLE extra (id INTEGER PRIMARY KEY);",
        );
        MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner second time")
            .run()
            .expect("apply 0002");

        assert_eq!(read_version(&conn), 2);
    }

    #[test]
    fn failing_migration_rolls_back_and_leaves_version_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "0001_initial.sql",
            "CREATE TABLE fixtures (id INTEGER PRIMARY KEY);",
        );
        // 0002 is intentionally bogus SQL — `execute_batch` will
        // surface this as a syntax error from SQLite.
        write_migration(&dir, "0002_broken.sql", "NOT VALID SQL AT ALL;");

        let mut conn = fresh_db();
        let result = MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner")
            .run();

        match result {
            Err(PersistenceError::MigrationFailed(msg)) => {
                assert!(
                    msg.contains("0002_broken.sql"),
                    "error must identify the failing file, got: {msg}"
                );
            }
            other => panic!("expected MigrationFailed, got {other:?}"),
        }

        // Version stays at 1 — the failed transaction did not commit.
        assert_eq!(read_version(&conn), 1);
        // The 0001 table is still there; the broken migration did
        // not touch it.
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='fixtures'",
                [],
                |row| row.get(0),
            )
            .expect("query fixtures presence");
        assert_eq!(count, 1);
    }

    #[test]
    fn version_above_highest_known_returns_schema_version_mismatch() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "0001_initial.sql",
            "CREATE TABLE fixtures (id INTEGER PRIMARY KEY);",
        );
        write_migration(
            &dir,
            "0002_extra.sql",
            "CREATE TABLE extra (id INTEGER PRIMARY KEY);",
        );

        let mut conn = fresh_db();
        // Seed `_meta` with a version above the highest fixture.
        conn.execute_batch(
            "CREATE TABLE _meta (schema_version INTEGER NOT NULL);
             INSERT INTO _meta (schema_version) VALUES (99);",
        )
        .expect("seed meta");

        let result = MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner")
            .run();

        match result {
            Err(PersistenceError::SchemaVersionMismatch(msg)) => {
                assert!(msg.contains("99"), "error must mention disk version: {msg}");
                assert!(msg.contains('2'), "error must mention highest known: {msg}");
            }
            other => panic!("expected SchemaVersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn missing_migrations_directory_maps_to_io_error() {
        let mut conn = fresh_db();
        let bogus = std::path::PathBuf::from("/this/path/does/not/exist/migrations");
        let result = MigrationRunner::new(&mut conn, &bogus);
        match result {
            Err(PersistenceError::IoError(_)) => {}
            other => panic!("expected IoError, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_version_prefix_is_rejected_before_any_sql_runs() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "0001_a.sql",
            "CREATE TABLE a (id INTEGER PRIMARY KEY);",
        );
        write_migration(
            &dir,
            "0001_b.sql",
            "CREATE TABLE b (id INTEGER PRIMARY KEY);",
        );

        let mut conn = fresh_db();
        let result = MigrationRunner::new(&mut conn, dir.path());
        match result {
            Err(PersistenceError::MigrationFailed(msg)) => {
                assert!(msg.contains("duplicate"), "got: {msg}");
                assert!(msg.contains("0001"), "got: {msg}");
            }
            other => panic!("expected MigrationFailed, got {other:?}"),
        }

        // No `_meta` row should have been written — discovery aborts
        // before any SQL runs.
        let meta_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='_meta'",
                [],
                |row| row.get(0),
            )
            .expect("query meta presence");
        assert_eq!(meta_exists, 0);
    }

    #[test]
    fn unparseable_version_prefix_surfaces_as_migration_failed() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "abc_not_a_number.sql",
            "CREATE TABLE x (id INTEGER PRIMARY KEY);",
        );

        let mut conn = fresh_db();
        let result = MigrationRunner::new(&mut conn, dir.path());
        match result {
            Err(PersistenceError::MigrationFailed(msg)) => {
                assert!(msg.contains("unparseable"), "got: {msg}");
            }
            other => panic!("expected MigrationFailed, got {other:?}"),
        }
    }

    #[test]
    fn non_sql_files_are_ignored_by_discovery() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "0001_initial.sql",
            "CREATE TABLE fixtures (id INTEGER PRIMARY KEY);",
        );
        // README and other ancillary files should not be treated as
        // migrations.
        write_migration(&dir, "README.md", "# notes");
        write_migration(&dir, "0002_draft.sql.bak", "SELECT 1;");

        let mut conn = fresh_db();
        MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner")
            .run()
            .expect("apply migrations");

        assert_eq!(read_version(&conn), 1);
    }

    #[test]
    fn empty_migrations_directory_only_creates_meta_table() {
        let dir = TempDir::new().expect("tempdir");

        let mut conn = fresh_db();
        MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner")
            .run()
            .expect("run on empty migrations");

        assert_eq!(read_version(&conn), 0);
    }

    #[test]
    fn migration_with_multiple_statements_applies_in_one_transaction() {
        let dir = TempDir::new().expect("tempdir");
        write_migration(
            &dir,
            "0001_compound.sql",
            "CREATE TABLE a (id INTEGER PRIMARY KEY);\n\
             CREATE TABLE b (id INTEGER PRIMARY KEY);\n\
             CREATE INDEX idx_a ON a(id);",
        );

        let mut conn = fresh_db();
        MigrationRunner::new(&mut conn, dir.path())
            .expect("construct runner")
            .run()
            .expect("apply compound migration");

        assert_eq!(read_version(&conn), 1);
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .expect("prepare")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"b".to_string()));
    }
}
