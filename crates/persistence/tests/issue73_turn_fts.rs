use std::fs;

use chrono::DateTime;
use openspace_persistence::{Database, SqliteTurnRepository};
use openspace_shared::ai::domain::{Part, Role, Turn};
use openspace_shared::id::{ChatId, TurnId, WorkspaceId};
use openspace_shared::persistence::TurnRepository;

fn migrations_dir() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")
}

fn now() -> chrono::DateTime<chrono::Utc> {
    DateTime::parse_from_rfc3339("2026-05-22T03:00:00Z")
        .expect("parse")
        .with_timezone(&chrono::Utc)
}

fn text_turn(id: TurnId, text: &str) -> Turn {
    Turn::new(
        id,
        Role::User,
        vec![Part::Text(text.to_string())],
        now(),
        None,
    )
}

async fn seeded_db(workspace_id: WorkspaceId, chat_id: ChatId) -> Database {
    let db = Database::open_in_memory(migrations_dir())
        .await
        .expect("open in-memory db");
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
                    "Issue 73",
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
    db
}

#[tokio::test]
async fn migration_creates_fts_table_and_triggers() {
    let db = Database::open_in_memory(migrations_dir())
        .await
        .expect("open in-memory db");
    db.connection()
        .call(|conn| {
            let table_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'turn_text_fts'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(table_count, 1);

            let trigger_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name IN ('trg_turn_text_fts_ai', 'trg_turn_text_fts_au', 'trg_turn_text_fts_ad')",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(trigger_count, 3);
            Ok(())
        })
        .await
        .expect("introspect schema");
}

#[tokio::test]
async fn insert_update_delete_keep_search_index_fresh() {
    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    let db = seeded_db(workspace_id, chat_id).await;
    let repo = SqliteTurnRepository::new(db.clone());
    let turn = text_turn(TurnId::new_v4(), "alpha original");

    repo.append(chat_id, &turn).await.expect("insert turn");
    assert_eq!(
        repo.search("original", 10).await.expect("search insert"),
        vec![(turn.id, chat_id)]
    );

    let updated = text_turn(turn.id, "beta updated");
    let payload_json = serde_json::to_string(&updated).expect("serialize updated turn");
    db.connection()
        .call(move |conn| {
            conn.execute(
                "UPDATE turns SET payload_json = ?1 WHERE id = ?2",
                rusqlite::params![payload_json, updated.id.as_uuid().to_string()],
            )?;
            Ok(())
        })
        .await
        .expect("update turn");
    assert!(repo
        .search("original", 10)
        .await
        .expect("search stale")
        .is_empty());
    assert_eq!(
        repo.search("updated", 10).await.expect("search updated"),
        vec![(turn.id, chat_id)]
    );

    db.connection()
        .call(move |conn| {
            conn.execute(
                "DELETE FROM turns WHERE id = ?1",
                [turn.id.as_uuid().to_string()],
            )?;
            Ok(())
        })
        .await
        .expect("delete turn");
    assert!(repo
        .search("updated", 10)
        .await
        .expect("search deleted")
        .is_empty());
}

#[tokio::test]
async fn migration_backfills_existing_turns_from_v1_database() {
    let temp = tempfile::tempdir().expect("tempdir");
    let v1_dir = temp.path().join("migrations-v1");
    fs::create_dir(&v1_dir).expect("create v1 migration dir");
    fs::copy(
        format!("{}/0001_initial.sql", migrations_dir()),
        v1_dir.join("0001_initial.sql"),
    )
    .expect("copy v1 migration");

    let db_path = temp.path().join("openspace.sqlite");
    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    let turn = text_turn(TurnId::new_v4(), "backfill searchable content");
    let turn_id = turn.id;
    let turn_for_insert = turn.clone();
    let db = Database::open(&db_path, &v1_dir).await.expect("open v1 db");
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
                    "Issue 73",
                    "",
                    "",
                    now().timestamp(),
                    now().timestamp()
                ],
            )?;
            conn.execute(
                "INSERT INTO turns (id, chat_id, kind, payload_json, sequence, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    turn_for_insert.id.as_uuid().to_string(),
                    chat_id.as_uuid().to_string(),
                    serde_json::to_string(&turn_for_insert.role).expect("serialize role"),
                    serde_json::to_string(&turn_for_insert).expect("serialize turn"),
                    1_i64,
                    now().timestamp()
                ],
            )?;
            Ok(())
        })
        .await
        .expect("seed v1 rows");
    drop(db);

    let migrated = Database::open(db_path, migrations_dir())
        .await
        .expect("apply v2 migration");
    let repo = SqliteTurnRepository::new(migrated);
    assert_eq!(
        repo.search("backfill", 10).await.expect("search backfill"),
        vec![(turn_id, chat_id)]
    );
}

#[tokio::test]
async fn search_supports_phrase_prefix_and_boolean_queries() {
    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    let db = seeded_db(workspace_id, chat_id).await;
    let repo = SqliteTurnRepository::new(db);
    let phrase = text_turn(TurnId::new_v4(), "hello world from search");
    let prefix = text_turn(TurnId::new_v4(), "helium prefix token");
    let boolean = text_turn(TurnId::new_v4(), "rustacean ferris shell");

    repo.append(chat_id, &phrase).await.expect("insert phrase");
    repo.append(chat_id, &prefix).await.expect("insert prefix");
    repo.append(chat_id, &boolean)
        .await
        .expect("insert boolean");

    assert_eq!(
        repo.search("\"hello world\"", 10).await.expect("phrase"),
        vec![(phrase.id, chat_id)]
    );
    assert_eq!(
        repo.search("hel*", 10).await.expect("prefix"),
        vec![(prefix.id, chat_id), (phrase.id, chat_id)]
    );
    assert_eq!(
        repo.search("rustacean NOT crab", 10)
            .await
            .expect("boolean"),
        vec![(boolean.id, chat_id)]
    );
}
