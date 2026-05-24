//! OpenSpace application entry point.
//!
//! At this scaffolding stage the binary only emits a placeholder banner
//! and exits with status `0`. Replacing this stub is the job of later
//! slices that wire up the GUI shell and the AI core.

use openspace_observability::file_logging::init_file_logging;
use openspace_persistence::DataDirs;

fn main() {
    let _file_logging = DataDirs::resolve()
        .ok()
        .and_then(|dirs| init_file_logging(dirs.root()).ok());
    println!("openspace v{} — placeholder", env!("CARGO_PKG_VERSION"));
}
