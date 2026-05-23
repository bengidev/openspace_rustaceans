use assert_cmd::Command;
use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn migrate_cli_reports_applies_and_preserves_dry_run_database() {
    let temp = tempdir().expect("tempdir");
    let db_path = temp.path().join("cli.sqlite");

    let status = migrate_command(&db_path)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status = String::from_utf8(status).expect("utf8 status");
    assert!(status.contains("schema_version: 0"));
    assert!(status.contains("pending:"));

    let before_dry_run = database_bytes(&db_path);
    let dry_run = migrate_command(&db_path)
        .arg("up")
        .arg("--dry-run")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let dry_run = String::from_utf8(dry_run).expect("utf8 dry-run");
    assert!(dry_run.contains("dry-run: no changes written"));
    assert_eq!(before_dry_run, database_bytes(&db_path));

    let up = migrate_command(&db_path)
        .arg("up")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let up = String::from_utf8(up).expect("utf8 up");
    assert!(up.contains("applied:"));

    let status = migrate_command(&db_path)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status = String::from_utf8(status).expect("utf8 final status");
    assert!(status.contains("schema_version: 2"));
    assert!(status.contains("pending: none"));
}

fn migrate_command(db_path: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("migrate").expect("migrate bin");
    command.arg(db_path);
    command
}

fn database_bytes(db_path: &std::path::Path) -> Vec<u8> {
    let conn = Connection::open(db_path).expect("open db for checkpoint");
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .expect("checkpoint wal");
    drop(conn);
    std::fs::read(db_path).expect("read db")
}
