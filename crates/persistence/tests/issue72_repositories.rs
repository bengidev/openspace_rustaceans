use chrono::DateTime;
use openspace_persistence::{
    Database, SqliteAttachmentStore, SqlitePermissionGrantStore, SqliteTurnRepository,
};
use openspace_shared::ai::domain::{Part, Role, Turn};
use openspace_shared::id::{AttachmentId, ChatId, PermissionGrantId, TurnId, WorkspaceId};
use openspace_shared::persistence::types::{
    Attachment, PermissionDecision, PermissionGrant, PermissionScope,
};
use openspace_shared::persistence::{AttachmentStore, PermissionGrantStore, TurnRepository};

fn migrations_dir() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")
}

fn now() -> chrono::DateTime<chrono::Utc> {
    DateTime::parse_from_rfc3339("2026-05-22T03:00:00Z")
        .expect("parse")
        .with_timezone(&chrono::Utc)
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
                    "Issue 72",
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

fn text_turn(id: TurnId, text: &str) -> Turn {
    Turn::new(
        id,
        Role::User,
        vec![Part::Text(text.to_string())],
        now(),
        None,
    )
}

#[tokio::test]
async fn turn_repository_round_trips_roots_children_and_orders_by_sequence() {
    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    let db = seeded_db(workspace_id, chat_id).await;
    let repo = SqliteTurnRepository::new(db.clone());

    let root = text_turn(TurnId::new_v4(), "root");
    let child = text_turn(TurnId::new_v4(), "child");

    repo.append(chat_id, &root).await.expect("insert root");
    repo.append_with_parent(chat_id, Some(root.id), &child)
        .await
        .expect("insert child");

    let turns = repo.list_by_chat(chat_id).await.expect("list turns");
    assert_eq!(turns, vec![root.clone(), child.clone()]);
    assert_eq!(
        repo.get(child.id).await.expect("get child"),
        Some(child.clone())
    );

    db.connection()
        .call(move |conn| {
            let deleted = conn.execute(
                "DELETE FROM turns WHERE id = ?1",
                [child.id.as_uuid().to_string()],
            )?;
            assert_eq!(deleted, 1);
            Ok(())
        })
        .await
        .expect("delete child");
    assert!(repo.get(child.id).await.expect("missing child").is_none());
}

#[tokio::test]
async fn attachment_store_round_trips_one_mebibyte_blob() {
    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    let db = seeded_db(workspace_id, chat_id).await;
    let turn_repo = SqliteTurnRepository::new(db.clone());
    let attachment_store = SqliteAttachmentStore::new(db);
    let turn = text_turn(TurnId::new_v4(), "with attachment");
    turn_repo.append(chat_id, &turn).await.expect("insert turn");

    let bytes = (0..(1024 * 1024))
        .map(|idx| (idx % 251) as u8)
        .collect::<Vec<_>>();
    let attachment = Attachment::new(
        AttachmentId::new_v4(),
        chat_id,
        Some(turn.id),
        "blob.bin".to_string(),
        "application/octet-stream".to_string(),
        bytes.len() as u64,
        "deterministic-pattern".to_string(),
        now(),
    );

    attachment_store
        .put(&attachment, &bytes)
        .await
        .expect("put attachment");
    assert_eq!(
        attachment_store
            .get_metadata(attachment.id)
            .await
            .expect("metadata"),
        Some(attachment.clone())
    );
    assert_eq!(
        attachment_store
            .get_bytes(attachment.id)
            .await
            .expect("bytes"),
        bytes
    );
    assert_eq!(
        attachment_store
            .list_for_chat(chat_id)
            .await
            .expect("list attachments"),
        vec![attachment.clone()]
    );
    attachment_store
        .delete(attachment.id)
        .await
        .expect("delete attachment");
    assert!(attachment_store
        .get_metadata(attachment.id)
        .await
        .expect("metadata removed")
        .is_none());
}

#[tokio::test]
async fn permission_grant_store_upserts_lists_and_revokes_without_duplicates() {
    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    let db = seeded_db(workspace_id, chat_id).await;
    let store = SqlitePermissionGrantStore::new(db.clone());
    let grant = PermissionGrant::new(
        PermissionGrantId::new_v4(),
        workspace_id,
        None,
        "network.fetch".to_string(),
        PermissionScope::Workspace,
        PermissionDecision::Allow,
        None,
        now(),
    );

    store.upsert(&grant).await.expect("first grant");
    store.upsert(&grant).await.expect("duplicate grant");
    assert_eq!(
        store.get(grant.id).await.expect("lookup"),
        Some(grant.clone())
    );
    assert_eq!(
        store
            .list_for_workspace(workspace_id)
            .await
            .expect("list grants"),
        vec![grant.clone()]
    );

    db.connection()
        .call(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM permission_grants WHERE workspace_id = ?1 AND tool_id = ?2 AND network_pattern = ?3",
                rusqlite::params![
                    workspace_id.as_uuid().to_string(),
                    "network.fetch",
                    grant.id.as_uuid().to_string()
                ],
                |row| row.get(0),
            )?;
            assert_eq!(count, 1);
            Ok(())
        })
        .await
        .expect("count grants");

    store.delete(grant.id).await.expect("revoke");
    assert!(store
        .get(grant.id)
        .await
        .expect("lookup after revoke")
        .is_none());
}

#[tokio::test]
async fn foreign_key_violations_surface_as_conflicting_writes() {
    let db = Database::open_in_memory(migrations_dir())
        .await
        .expect("open in-memory db");
    let turn_repo = SqliteTurnRepository::new(db.clone());
    let missing_chat = turn_repo
        .append(ChatId::new_v4(), &text_turn(TurnId::new_v4(), "orphan"))
        .await
        .expect_err("missing chat rejected");
    assert!(matches!(
        missing_chat,
        openspace_shared::persistence::PersistenceError::ConflictingWrite(_)
    ));

    let workspace_id = WorkspaceId::new_v4();
    let chat_id = ChatId::new_v4();
    let db = seeded_db(workspace_id, chat_id).await;
    let turn_repo = SqliteTurnRepository::new(db);
    let missing_parent = turn_repo
        .append_with_parent(
            chat_id,
            Some(TurnId::new_v4()),
            &text_turn(TurnId::new_v4(), "orphan child"),
        )
        .await
        .expect_err("missing parent rejected");
    assert!(matches!(
        missing_parent,
        openspace_shared::persistence::PersistenceError::ConflictingWrite(_)
    ));
}
