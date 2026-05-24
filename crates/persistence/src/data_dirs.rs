//! Platform-conventional paths for persistence artefacts.

use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use openspace_shared::persistence::PersistenceError;
use static_assertions::assert_impl_all;

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "openspace_desktop";
const SETTINGS_FILE: &str = "settings.toml";
const DATA_DB_FILE: &str = "data.db";
const THEMES_DIR: &str = "themes";
const LOGS_DIR: &str = "logs";
const CACHE_DIR: &str = "cache";

/// Resolved filesystem locations consumed by the persistence layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    root: PathBuf,
}

impl DataDirs {
    /// Resolve the platform-conventional data root and ensure child directories exist.
    pub fn resolve() -> Result<Self, PersistenceError> {
        let project_dirs =
            ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).ok_or_else(|| {
                PersistenceError::IoError(
                    "could not resolve platform data directory for openspace_desktop".to_string(),
                )
            })?;

        Self::with_root(project_dirs.data_dir())
    }

    /// Build from an explicit root, primarily for tests.
    pub fn with_root(root: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        let dirs = Self {
            root: root.as_ref().to_path_buf(),
        };
        dirs.ensure_dirs()?;
        Ok(dirs)
    }

    /// Path to the data root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to `settings.toml`; file is not created by this type.
    #[must_use]
    pub fn settings_toml(&self) -> PathBuf {
        self.root.join(SETTINGS_FILE)
    }

    /// Path to `data.db`; file is not created by this type.
    #[must_use]
    pub fn data_db(&self) -> PathBuf {
        self.root.join(DATA_DB_FILE)
    }

    /// Path to the theme directory.
    #[must_use]
    pub fn themes_dir(&self) -> PathBuf {
        self.root.join(THEMES_DIR)
    }

    /// Path to the logs directory.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join(LOGS_DIR)
    }

    /// Path to the cache directory.
    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join(CACHE_DIR)
    }

    fn ensure_dirs(&self) -> Result<(), PersistenceError> {
        fs::create_dir_all(self.themes_dir())?;
        fs::create_dir_all(self.logs_dir())?;
        fs::create_dir_all(self.cache_dir())?;
        Ok(())
    }
}

assert_impl_all!(DataDirs: Clone, Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_root_creates_subdirectories_and_returns_typed_paths() {
        let temp_dir = tempfile::tempdir().expect("temp dir");

        let dirs = DataDirs::with_root(temp_dir.path()).expect("data dirs");

        assert_eq!(dirs.settings_toml(), temp_dir.path().join("settings.toml"));
        assert_eq!(dirs.data_db(), temp_dir.path().join("data.db"));
        assert_eq!(dirs.themes_dir(), temp_dir.path().join("themes"));
        assert_eq!(dirs.logs_dir(), temp_dir.path().join("logs"));
        assert_eq!(dirs.cache_dir(), temp_dir.path().join("cache"));
        assert!(dirs.themes_dir().is_dir());
        assert!(dirs.logs_dir().is_dir());
        assert!(dirs.cache_dir().is_dir());
        assert!(!dirs.settings_toml().exists());
        assert!(!dirs.data_db().exists());
    }

    #[test]
    fn with_root_is_idempotent() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let first = DataDirs::with_root(temp_dir.path()).expect("first resolve");
        let second = DataDirs::with_root(temp_dir.path()).expect("second resolve");

        assert_eq!(first, second);
        assert!(second.themes_dir().is_dir());
        assert!(second.logs_dir().is_dir());
        assert!(second.cache_dir().is_dir());
    }

    #[test]
    fn resolve_uses_platform_conventional_root() {
        let dirs = DataDirs::resolve().expect("resolve data dirs");
        let expected = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .expect("platform project dirs")
            .data_dir()
            .to_path_buf();

        assert_eq!(dirs.settings_toml(), expected.join("settings.toml"));
        assert_eq!(dirs.data_db(), expected.join("data.db"));
        assert_eq!(dirs.themes_dir(), expected.join("themes"));
        assert_eq!(dirs.logs_dir(), expected.join("logs"));
        assert_eq!(dirs.cache_dir(), expected.join("cache"));
    }
}
