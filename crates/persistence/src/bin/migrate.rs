//! Developer migration helper for local SQLite databases.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use openspace_persistence::{Database, MigrationRunner};
use rusqlite::Connection;

#[derive(Debug, Parser)]
#[command(about = "Inspect and apply openspace-persistence migrations")]
struct Cli {
    /// SQLite database path to inspect or migrate.
    db_path: PathBuf,

    /// Override the bundled migrations directory.
    #[arg(long, value_name = "PATH", global = true)]
    migrations_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print current schema version and pending migrations.
    Status,
    /// Apply pending migrations.
    Up {
        /// Print migrations that would be applied without touching the database.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let migrations_dir = cli.migrations_dir.unwrap_or_else(bundled_migrations_dir);

    match cli.command {
        Command::Status => print_status(&cli.db_path, &migrations_dir),
        Command::Up { dry_run: true } => print_dry_run(&cli.db_path, &migrations_dir),
        Command::Up { dry_run: false } => apply_migrations(&cli.db_path, &migrations_dir).await,
    }
}

fn print_status(db_path: &Path, migrations_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = Connection::open(db_path)?;
    let mut runner = MigrationRunner::new(&mut conn, migrations_dir)?;
    let current = runner.current_version()?;
    let pending = runner.pending_migrations()?;

    println!("schema_version: {current}");
    print_pending(&pending);

    Ok(())
}

fn print_dry_run(db_path: &Path, migrations_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn =
        Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut runner = MigrationRunner::new(&mut conn, migrations_dir)?;
    let current = runner.current_version()?;
    let pending = runner.pending_migrations()?;

    println!("schema_version: {current}");
    println!("dry-run: no changes written");
    print_pending(&pending);

    Ok(())
}

async fn apply_migrations(
    db_path: &Path,
    migrations_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let before = pending_for_write(db_path, migrations_dir)?;
    Database::open(db_path.to_path_buf(), migrations_dir).await?;

    if before.is_empty() {
        println!("applied: none");
    } else {
        println!("applied:");
        for (version, file_name) in before {
            println!("  {version}: {file_name}");
        }
    }

    Ok(())
}

fn pending_for_write(
    db_path: &Path,
    migrations_dir: &Path,
) -> Result<Vec<(i64, String)>, Box<dyn std::error::Error>> {
    let mut conn = Connection::open(db_path)?;
    let mut runner = MigrationRunner::new(&mut conn, migrations_dir)?;
    Ok(runner.pending_migrations()?)
}

fn print_pending(pending: &[(i64, String)]) {
    if pending.is_empty() {
        println!("pending: none");
    } else {
        println!("pending:");
        for (version, file_name) in pending {
            println!("  {version}: {file_name}");
        }
    }
}

fn bundled_migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
}
