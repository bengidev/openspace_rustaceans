use chrono::DateTime;
use openspace_persistence::{Database, SqliteTurnRepository};
use openspace_shared::ai::domain::{Part, Role, Turn};
use openspace_shared::id::{ChatId, TurnId, WorkspaceId};
use openspace_shared::persistence::{PersistenceError, TurnRepository};
use rusqlite::Connection;
use std::sync::Arc;

fn migrations_dir() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")
}

fn now() -> chrono::DateTime<chrono::Utc> {
    DateTime::parse_from_rfc3339("2026-05-22T03:00:00Z")
        .expect("parse")
        .with_timezone(&chrono::Utc)
}

fn text_turn(text: String) -> Turn {
    Turn::new(
        TurnId::new_v4(),
        Role::User,
        vec![Part::Text(text)],
        now(),
        None,
    )
}

async fn seed(db: &Database, workspace_id: WorkspaceId, chat_id: ChatId) {
    db.connection()
        .call(move |conn| {
            conn.execute(
                "INSERT INTO workspaces (id, root_path, last_opened_at, trust_mode, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    workspace_id.as_uuid().to_string(),
                    "/tmp/openspace",
                    now().timestamp(),
                    "trusted",
                    now().timestamp()
                ],
            )?;
            conn.execute(
                "INSERT INTO chats (id, workspace_id, title, model_provider, model_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    chat_id.as_uuid().to_string(),
                    workspace_id.as_uuid().to_string(),
                    "Issue 74",
                    "",
                    "",
                    now().timestamp(),
                    now().timestamp()
                ],
            )?;
            Ok(())
        })
        .await
        .expect("seed ancestors");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_turn_writes_complete_without_dropping_rows() {
    const TASKS: usize = 8;
    const ROWS_PER_TASK: usize = 50;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("db.sqlite3");
    let db = Database::open(&db_path, migrations_dir())
        .await
        .expect("open db");
    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    seed(&db, workspace_id, chat_id).await;

    let repo = Arc::new(SqliteTurnRepository::new(db.clone()));
    let mut handles = Vec::new();
    for task in 0..TASKS {
        let repo = Arc::clone(&repo);
        handles.push(tokio::spawn(async move {
            for row in 0..ROWS_PER_TASK {
                repo.append(chat_id, &text_turn(format!("{task}-{row}")))
                    .await?;
            }
            Ok::<(), PersistenceError>(())
        }));
    }

    for handle in handles {
        handle.await.expect("task join").expect("task write");
    }

    let count: i64 = db
        .connection()
        .call(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM turns WHERE chat_id = ?1",
                [chat_id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .map_err(tokio_rusqlite::Error::from)
        })
        .await
        .expect("count turns");
    assert_eq!(count, (TASKS * ROWS_PER_TASK) as i64);
}

#[tokio::test]
async fn sustained_busy_surfaces_database_locked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("db.sqlite3");
    let db = Database::open(&db_path, migrations_dir())
        .await
        .expect("open db");
    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    seed(&db, workspace_id, chat_id).await;

    let lock_conn = Connection::open(&db_path).expect("open lock conn");
    lock_conn
        .execute_batch("PRAGMA busy_timeout = 0; BEGIN EXCLUSIVE;")
        .expect("hold write lock");

    let repo = SqliteTurnRepository::new(db);
    let err = repo
        .append(chat_id, &text_turn("locked".to_string()))
        .await
        .expect_err("lock should exceed retry budget");

    match err {
        PersistenceError::DatabaseLocked(_) => {}
        other => panic!("expected DatabaseLocked, got {other:?}"),
    }
}
