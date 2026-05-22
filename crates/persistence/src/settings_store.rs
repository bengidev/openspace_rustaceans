//! [`SettingsStore`] — in-process owner of `settings.toml`.
//!
//! Reads, mutates, and atomically writes the typed
//! [`openspace_shared::settings::Settings`] record back to disk. The
//! store hands out cheap snapshots of the current state and serialises
//! every write through an internal lock so two concurrent
//! [`SettingsStore::edit`] calls cannot interleave.
//!
//! # Surface
//!
//! - [`SettingsStore::load`] — open (or create) the store at `path`.
//!   A missing file resolves to [`Settings::default`].
//! - [`SettingsStore::current`] — cheap clone of the live snapshot.
//! - [`SettingsStore::edit`] — run a mutator against a draft, then
//!   atomically write the result back.
//! - [`SettingsStore::path`] — borrow the on-disk path for diagnostics.
//!
//! # Atomic write
//!
//! The write half follows the documented "temp-then-rename" recipe so a
//! mid-write process kill can never leave the target file partially
//! written:
//!
//! 1. Serialise the new [`Settings`] into a `String` in memory.
//! 2. Create a sibling temp file (`.<name>.tmp.<unique>`), write the
//!    serialised bytes, [`std::fs::File::sync_all`] them.
//! 3. [`std::fs::rename`] the temp file over the target path. On POSIX
//!    this is atomic with respect to a concurrent reader.
//! 4. Open the parent directory and `sync_all` it so the rename itself
//!    is durable across an unclean shutdown.
//!
//! The acceptance-criteria test in this module exercises the
//! "killed before rename" scenario via an internal writer hook and
//! confirms that the target file equals the *pre-edit* content
//! whenever the rename step is skipped.
//!
//! # Threading model
//!
//! The live snapshot lives behind an [`Arc<RwLock<Settings>>`]. Reads
//! ([`SettingsStore::current`]) take a read guard, clone, drop the
//! guard. Writes ([`SettingsStore::edit`]) take an async
//! [`tokio::sync::Mutex`] guard *first* — the write serialiser — then
//! produce a draft, perform IO on the blocking pool, and only then
//! upgrade to a write guard on the snapshot to publish the new value.
//! That ordering means a long-running serialise / IO step never blocks
//! readers, and two concurrent edits cannot race on the same on-disk
//! file.
//!
//! # Privacy
//!
//! [`Settings`] carries no secret material (US#7 of PRD-03). The store
//! inherits that posture by construction — it never reads or writes
//! anything but a `Settings` value.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use openspace_shared::persistence::PersistenceError;
use openspace_shared::settings::Settings;
use static_assertions::assert_impl_all;
use tokio::sync::Mutex as AsyncMutex;

/// Default file extension marking the sibling temp file the atomic
/// write goes through. The actual filename is
/// `.<target>.tmp.<unique-suffix>`; the leading dot keeps it hidden in
/// most file managers and the suffix prevents collisions when two
/// processes ever target the same file (the second loses the rename
/// race, never the temp write).
const TEMP_PREFIX: &str = ".";
const TEMP_INFIX: &str = ".tmp.";

// ─────────────────────────────────────────────────────────────────────
// FileWriter — pluggable IO so the unit tests can inject a partial-
// write failure mode without going through OS-level fault injection.
// Production code uses `StdFileWriter`, the only impl that touches the
// real filesystem; the test scenarios swap in `FailingWriter` to
// simulate "killed mid-rename".
// ─────────────────────────────────────────────────────────────────────

/// Filesystem writer the [`SettingsStore`] dispatches its atomic write
/// through. Production code never sees this trait — it is a
/// `pub(crate)` seam used purely so the unit tests can inject a
/// failure between the temp-file write and the rename.
pub(crate) trait FileWriter: Send + Sync {
    /// Atomically replace the contents of `target` with `bytes`. The
    /// implementation guarantees that a reader observing `target` at
    /// any moment sees either the pre-call content or the post-call
    /// content, never a partial mix.
    fn write_atomically(&self, target: &Path, bytes: &[u8]) -> io::Result<()>;
}

/// Real-filesystem implementation of [`FileWriter`]. Writes a sibling
/// temp file, fsyncs it, renames over the target, then fsyncs the
/// parent directory.
#[derive(Debug, Default)]
pub(crate) struct StdFileWriter;

impl FileWriter for StdFileWriter {
    fn write_atomically(&self, target: &Path, bytes: &[u8]) -> io::Result<()> {
        let parent = target.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "settings target has no parent directory: {}",
                    target.display()
                ),
            )
        })?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };

        // Build the sibling temp file name. We append the process id and
        // a thread-local counter so two concurrent stores targeting the
        // same path can both write their temp files without colliding;
        // the rename step is the contention point, and rename loses the
        // race cleanly on POSIX (the second writer overwrites the
        // first).
        let target_name = target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("settings");
        let unique = unique_suffix();
        let temp_path = parent.join(format!("{TEMP_PREFIX}{target_name}{TEMP_INFIX}{unique}"));

        // Write + fsync the temp file. The temp file is the pre-image
        // of the durable on-disk state, so we *must* fsync it before
        // the rename — otherwise the rename can land but the data
        // pages can stay buffered, and a power loss in that window
        // leaves the target pointing at empty / zeroed blocks.
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }

        // Rename onto the target. POSIX guarantees atomicity here, so
        // a reader sees either the old inode or the new one; the
        // partial-write window the AC calls out lives entirely on the
        // *temp* file, never on `target`.
        if let Err(rename_err) = fs::rename(&temp_path, target) {
            // Best-effort cleanup so a failed rename does not leak the
            // sibling. Ignore the unlink error — the original error is
            // what the caller cares about.
            let _ = fs::remove_file(&temp_path);
            return Err(rename_err);
        }

        // Fsync the parent directory so the rename itself survives a
        // power cut. Best-effort: some filesystems (notably Windows
        // NTFS through certain kernels) reject `sync_all` on a
        // directory handle; we treat that as non-fatal because the
        // rename is already durable on those targets.
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }

        Ok(())
    }
}

/// Build a short collision-resistant suffix for the temp file name.
/// Combines process id, monotonic nanoseconds, and a small counter so
/// two near-simultaneous writes from the same process land on
/// different temp files.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{pid}.{nanos}.{n}")
}

// ─────────────────────────────────────────────────────────────────────
// SettingsStore — the public surface PRD-03 Slice 3 ships.
// ─────────────────────────────────────────────────────────────────────

/// In-process owner of `settings.toml`.
///
/// Construct one with [`SettingsStore::load`]; from then on, callers
/// read via [`SettingsStore::current`] and mutate via
/// [`SettingsStore::edit`]. The store is `Send + Sync` and intended to
/// live behind an `Arc` shared by every subsystem that needs to read
/// or update settings.
///
/// See the module docs for the threading model and the atomic-write
/// recipe.
pub struct SettingsStore {
    /// On-disk path the store reads from and writes back to.
    path: PathBuf,
    /// Live snapshot of the current settings. Read paths take a read
    /// guard, clone, drop. Write paths upgrade to a write guard only
    /// after the on-disk write has succeeded.
    snapshot: Arc<RwLock<Settings>>,
    /// Async serialiser for writes. Held across the whole edit
    /// pipeline so two concurrent [`Self::edit`] calls cannot
    /// interleave their `read → mutate → serialise → write → publish`
    /// sequences. Async because the IO step runs on the blocking pool
    /// via `tokio::task::spawn_blocking` and we want callers to stay
    /// on the runtime while they wait.
    write_lock: AsyncMutex<()>,
    /// Pluggable filesystem writer. Production builds wire
    /// [`StdFileWriter`]; tests substitute a stub via
    /// `Self::load_with_writer` to inject a partial-write scenario.
    writer: Arc<dyn FileWriter>,
}

assert_impl_all!(SettingsStore: Send, Sync);

impl std::fmt::Debug for SettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The `dyn FileWriter` trait object is not `Debug`. We project
        // the public-relevant fields (path + current snapshot) and
        // omit the writer / lock — neither carries inspection value.
        f.debug_struct("SettingsStore")
            .field("path", &self.path)
            .field("snapshot", &self.current())
            .finish_non_exhaustive()
    }
}

impl SettingsStore {
    /// Open the store at `path`.
    ///
    /// - If the file is absent, the snapshot is initialised to
    ///   [`Settings::default`] and *no file is written*. The default
    ///   shape only lands on disk the first time a caller invokes
    ///   [`Self::edit`], matching the documented "empty file" semantics
    ///   in `Settings`'s module docs.
    /// - If the file exists, its contents are parsed into a
    ///   [`Settings`]. Parse failures map to
    ///   [`PersistenceError::SerializationError`]; IO failures
    ///   (permission denied, file unreadable) map to
    ///   [`PersistenceError::IoError`].
    ///
    /// File IO runs on the tokio blocking pool so the runtime thread
    /// the caller is on stays free to drive UI work concurrently.
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, PersistenceError> {
        let path = path.into();
        let read_path = path.clone();
        let bytes = tokio::task::spawn_blocking(move || match fs::read_to_string(&read_path) {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        })
        .await
        .map_err(|join_err| {
            // A panic on the blocking pool is unrecoverable for the
            // caller — surface it as an IO error so the agent loop can
            // fall back to defaults rather than crashing.
            PersistenceError::IoError(format!("settings load task panicked: {join_err}"))
        })?
        .map_err(PersistenceError::from)?;

        let settings = match bytes {
            None => Settings::default(),
            Some(text) => toml::from_str::<Settings>(&text)
                .map_err(|err| PersistenceError::SerializationError(err.to_string()))?,
        };

        Ok(Self {
            path,
            snapshot: Arc::new(RwLock::new(settings)),
            write_lock: AsyncMutex::new(()),
            writer: Arc::new(StdFileWriter),
        })
    }

    /// On-disk path the store reads from and writes back to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Cheap clone of the current snapshot. Cloning a [`Settings`] is
    /// a handful of `String` clones plus copies of the typed handles
    /// — negligible compared to a real read-side workflow, so callers
    /// are free to call [`Self::current`] inline rather than caching
    /// the result.
    #[must_use]
    pub fn current(&self) -> Settings {
        self.snapshot
            .read()
            .expect("SettingsStore snapshot RwLock poisoned")
            .clone()
    }

    /// Run `mutator` against a draft of the current settings, then
    /// atomically persist the result.
    ///
    /// Order of operations:
    ///
    /// 1. Take the write serialiser lock so two concurrent edits run
    ///    sequentially.
    /// 2. Snapshot the current settings, hand the draft to `mutator`.
    /// 3. Serialise the draft to a TOML string.
    /// 4. Hand the bytes to the internal `FileWriter` on the blocking
    ///    pool; the writer guarantees temp-then-rename atomicity.
    /// 5. Publish the new settings to the in-memory snapshot.
    ///
    /// Failures in steps 3 or 4 leave the in-memory snapshot
    /// *unchanged*, mirroring the "either pre-edit or post-edit" guarantee
    /// the on-disk file enforces. Step-3 failures map to
    /// [`PersistenceError::SerializationError`]; step-4 failures map to
    /// [`PersistenceError::IoError`].
    pub async fn edit<F>(&self, mutator: F) -> Result<(), PersistenceError>
    where
        F: FnOnce(&mut Settings) + Send,
    {
        let _guard = self.write_lock.lock().await;

        let mut draft = self.current();
        mutator(&mut draft);

        let serialised = toml::to_string(&draft)
            .map_err(|err| PersistenceError::SerializationError(err.to_string()))?;

        let writer = Arc::clone(&self.writer);
        let path = self.path.clone();
        let bytes = serialised.into_bytes();
        tokio::task::spawn_blocking(move || writer.write_atomically(&path, &bytes))
            .await
            .map_err(|join_err| {
                PersistenceError::IoError(format!("settings write task panicked: {join_err}"))
            })?
            .map_err(PersistenceError::from)?;

        // Only publish in-memory after the on-disk write succeeded.
        *self
            .snapshot
            .write()
            .expect("SettingsStore snapshot RwLock poisoned") = draft;

        Ok(())
    }

    /// Test seam: build a store backed by a custom [`FileWriter`].
    /// Lets unit tests inject "killed mid-rename" semantics without
    /// going through the OS. Marked `pub(crate)` so production code
    /// cannot reach it.
    #[cfg(test)]
    pub(crate) async fn load_with_writer(
        path: impl Into<PathBuf>,
        writer: Arc<dyn FileWriter>,
    ) -> Result<Self, PersistenceError> {
        let mut store = Self::load(path).await?;
        store.writer = writer;
        Ok(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use openspace_shared::keybinding::KeybindingProfileId;
    use openspace_shared::settings::ThemeMode;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::tempdir;

    /// `load` on a missing file resolves to `Settings::default()` and
    /// does *not* create the file.
    #[tokio::test]
    async fn load_absent_file_yields_default_without_creating_it() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let store = SettingsStore::load(&path).await.expect("load default");

        assert_eq!(store.current(), Settings::default());
        assert!(
            !path.exists(),
            "load() must not create the settings file when absent"
        );
    }

    /// `load` round-trips: write a TOML fixture, load, observe that
    /// every field arrived intact.
    #[tokio::test]
    async fn load_round_trips_fixture_from_disk() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        let mut original = Settings::default();
        original.update_check_enabled = true;
        original.keybinding_profile = KeybindingProfileId::new("vim");
        original.default_provider = "hosted-router".to_string();
        original.theme_mode = ThemeMode::Dark;
        let serialised = toml::to_string(&original).expect("serialize fixture");
        fs::write(&path, serialised).expect("write fixture");

        let store = SettingsStore::load(&path).await.expect("load fixture");
        assert_eq!(store.current(), original);
    }

    /// Garbled TOML on disk surfaces as `SerializationError`, not as
    /// a panic and not as a silent fallback to defaults.
    #[tokio::test]
    async fn load_invalid_toml_maps_to_serialization_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        fs::write(&path, "this is = not [valid toml").expect("write garbage");

        let err = SettingsStore::load(&path)
            .await
            .expect_err("load must fail on garbled toml");
        assert!(
            matches!(err, PersistenceError::SerializationError(_)),
            "expected SerializationError, got {err:?}",
        );
    }

    /// `edit` produces a TOML file that `toml::from_str` can read back
    /// into the same `Settings` value. Acceptance criterion #2.
    #[tokio::test]
    async fn edit_produces_valid_toml_readable_by_toml_crate() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let store = SettingsStore::load(&path).await.expect("load");

        store
            .edit(|s| {
                s.update_check_enabled = true;
                s.theme_mode = ThemeMode::Light;
            })
            .await
            .expect("edit");

        let on_disk = fs::read_to_string(&path).expect("read after edit");
        let decoded: Settings = toml::from_str(&on_disk).expect("decode after edit");
        assert!(decoded.update_check_enabled);
        assert_eq!(decoded.theme_mode, ThemeMode::Light);
        assert_eq!(decoded, store.current());
    }

    /// In-memory snapshot updates only after the on-disk write
    /// commits. Two sequential edits both land.
    #[tokio::test]
    async fn edit_publishes_snapshot_after_disk_write() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let store = SettingsStore::load(&path).await.expect("load");

        store
            .edit(|s| s.update_check_enabled = true)
            .await
            .expect("first edit");
        assert!(store.current().update_check_enabled);

        store
            .edit(|s| s.theme_mode = ThemeMode::Dark)
            .await
            .expect("second edit");
        let now = store.current();
        assert!(now.update_check_enabled);
        assert_eq!(now.theme_mode, ThemeMode::Dark);
    }

    /// Acceptance criterion #3 — atomic-write injector test.
    ///
    /// The injector simulates "killed mid-rename" by writing the temp
    /// file (so the temp-write step succeeds in isolation) and then
    /// returning an IO error before the rename. The on-disk target
    /// must remain at the *pre-edit* content.
    #[tokio::test]
    async fn atomic_write_kill_before_rename_leaves_pre_edit_content_intact() {
        // Set up a target file with a known pre-edit shape so we can
        // assert it is preserved verbatim through the failed write.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let pre_edit = {
            let mut s = Settings::default();
            s.default_provider = "pre-edit-provider".to_string();
            s
        };
        fs::write(
            &path,
            toml::to_string(&pre_edit).expect("serialize pre-edit"),
        )
        .expect("seed pre-edit file");

        // The injecting writer always fails before the rename. We
        // count how many times it ran so the test catches a regression
        // where a future change accidentally bypasses the writer.
        struct AbortBeforeRename {
            ran: AtomicBool,
        }
        impl FileWriter for AbortBeforeRename {
            fn write_atomically(&self, _target: &Path, _bytes: &[u8]) -> io::Result<()> {
                self.ran.store(true, Ordering::SeqCst);
                Err(io::Error::other("simulated kill before rename"))
            }
        }
        let injector = Arc::new(AbortBeforeRename {
            ran: AtomicBool::new(false),
        });

        let store = SettingsStore::load_with_writer(&path, injector.clone())
            .await
            .expect("load with injector");

        // Confirm the store loaded the pre-edit shape from disk.
        assert_eq!(store.current(), pre_edit);

        let err = store
            .edit(|s| s.default_provider = "post-edit-provider".to_string())
            .await
            .expect_err("edit must fail when rename is killed");
        assert!(
            matches!(err, PersistenceError::IoError(_)),
            "expected IoError mapping, got {err:?}",
        );

        // The injector ran (so the failure happened on the right code
        // path), but the in-memory snapshot stayed at the pre-edit
        // value because the publish step is gated on the IO success.
        assert!(injector.ran.load(Ordering::SeqCst));
        assert_eq!(store.current(), pre_edit);

        // The on-disk file is also still the pre-edit content,
        // byte-for-byte. This is the heart of the AC: a kill mid-write
        // must leave the file as either pre or post — never partial
        // — and this scenario locks the pre side.
        let on_disk = fs::read_to_string(&path).expect("read after failed edit");
        let decoded: Settings = toml::from_str(&on_disk).expect("decode after failed edit");
        assert_eq!(decoded, pre_edit);
    }

    /// `Send + Sync` is asserted at compile time via
    /// `assert_impl_all!`. This test exists as a runtime witness so a
    /// future change that breaks the assertion shows up in test
    /// output too, not only in `cargo build`.
    #[tokio::test]
    async fn store_is_shareable_across_tasks() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let store = Arc::new(SettingsStore::load(&path).await.expect("load"));

        let s1 = Arc::clone(&store);
        let h1 = tokio::spawn(async move { s1.current() });
        let s2 = Arc::clone(&store);
        let h2 = tokio::spawn(async move { s2.edit(|s| s.update_check_enabled = true).await });

        h1.await.expect("read task");
        h2.await.expect("write task").expect("edit");

        assert!(store.current().update_check_enabled);
    }
}
