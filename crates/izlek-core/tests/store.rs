//! Integration tests for izlek-core: the storage boundary driven through the
//! Turso implementation, and the account flows on top of it.
//!
//! New integration tests belong in this file rather than a new `tests/*.rs`:
//! one test binary links and runs once.

use std::path::PathBuf;

use izlek_core::Role;
use izlek_core::auth::{Token, hash_password};
use izlek_core::store::{
    Audience, Event, MailOutcome, MailRule, NewAttachment, NewSender, NewUser, SendKind, SendState,
    Store, StoreError, Trigger, TursoStore, User,
};
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

/// A throwaway database on disk. Turso's in-memory mode is not what production
/// runs, so the tests exercise a real file.
struct Scratch {
    dir: PathBuf,
    store: TursoStore,
}

impl Scratch {
    async fn open() -> Self {
        let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TursoStore::open(dir.join("izlek.db").to_str().unwrap())
            .await
            .unwrap();
        Self { dir, store }
    }
}

/// A second, independent connection to the scratch database, for tests that
/// need to see a column the way it actually sits on disk rather than through
/// [`Store`]'s API.
async fn raw_conn(scratch: &Scratch) -> turso::Connection {
    let db = turso::Builder::new_local(scratch.dir.join("izlek.db").to_str().unwrap())
        .build()
        .await
        .unwrap();
    db.connect().unwrap()
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn workspace_with_admin() -> (Scratch, String, String) {
    let scratch = Scratch::open().await;
    let (ws, admin) = scratch
        .store
        .claim_workspace(
            "İzlek",
            "ada@izlek.sh",
            "Ada",
            &hash_password("tide-tables-1892").unwrap(),
        )
        .await
        .unwrap();
    (scratch, ws.id, admin.id)
}

/// Claims a workspace on an arbitrary store, for the tests that need one
/// without the `Scratch` wrapper.
async fn claim(store: &TursoStore) -> (String, String) {
    let (ws, admin) = store
        .claim_workspace(
            "İzlek",
            "ada@izlek.sh",
            "Ada",
            &hash_password("tide-tables-1892").unwrap(),
        )
        .await
        .unwrap();
    (ws.id, admin.id)
}

async fn member(store: &TursoStore, workspace_id: &str, email: &str, name: &str) -> String {
    store
        .create_user(NewUser {
            workspace_id: workspace_id.to_string(),
            email: email.into(),
            display_name: name.into(),
            role: Role::Member,
            invited_by: None,
        })
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn the_schema_is_created_once_and_survives_reopen() {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();

    let first = TursoStore::open(&path).await.unwrap();
    claim(&first).await;
    drop(first);

    // Re-opening a database that already has tables must not re-run the
    // schema (its CREATE TABLE would fail on the first one) and must not
    // lose what the first open wrote.
    let second = TursoStore::open(&path).await.unwrap();
    assert_eq!(second.workspace().await.unwrap().unwrap().name, "İzlek");
    drop(second);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_subtask_is_a_task_with_a_parent() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let parent = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship the exporter",
        None,
        &admin,
    )
    .await;

    let board = scratch.store.board(&workspace).await.unwrap().unwrap();
    let column_id = column_named(&scratch.store, &workspace, "Backlog").await;
    let child = scratch
        .store
        .create_task(NewTask {
            clock_at: None,
            board_id: &board.id,
            column_id: &column_id,
            parent_id: Some(&parent),
            title: "Write the CSV writer",
            description: "",
            deadline: None,
            created_by: &admin,
        })
        .await
        .unwrap()
        .row;

    // It gets a key of its own from the same prefix, not a derived DZ-14.1.
    assert!(child.task_key.starts_with("DZ-"));
    assert_ne!(child.task_key, parent);

    let children = scratch.store.subtasks(&parent).await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, child.id);
    assert!(
        scratch.store.subtasks(&child.id).await.unwrap().is_empty(),
        "a subtask has none of its own"
    );
}

#[tokio::test]
async fn subtasks_go_one_level_deep() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let board = scratch.store.board(&workspace).await.unwrap().unwrap();
    let column_id = column_named(&scratch.store, &workspace, "Backlog").await;

    let parent = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Parent",
        None,
        &admin,
    )
    .await;
    let child = scratch
        .store
        .create_task(NewTask {
            clock_at: None,
            board_id: &board.id,
            column_id: &column_id,
            parent_id: Some(&parent),
            title: "Child",
            description: "",
            deadline: None,
            created_by: &admin,
        })
        .await
        .unwrap()
        .row
        .id;

    // Creating under a subtask is refused outright.
    let grandchild = scratch
        .store
        .create_task(NewTask {
            clock_at: None,
            board_id: &board.id,
            column_id: &column_id,
            parent_id: Some(&child),
            title: "Grandchild",
            description: "",
            deadline: None,
            created_by: &admin,
        })
        .await;
    assert!(matches!(grandchild, Err(StoreError::NotNestable)));

    // And so is reaching the same shape from the other side: an existing task
    // cannot be filed under a subtask ...
    let loose = add_task(&scratch.store, &workspace, "Backlog", "Loose", None, &admin).await;
    assert!(matches!(
        scratch.store.set_parent(&loose, Some(&child)).await,
        Err(StoreError::NotNestable)
    ));

    // ... nor can a task that already has children become one itself.
    assert!(matches!(
        scratch.store.set_parent(&parent, Some(&loose)).await,
        Err(StoreError::NotNestable)
    ));

    // Nothing was written by any of the three refusals.
    assert_eq!(scratch.store.subtasks(&parent).await.unwrap().len(), 1);
    assert!(scratch.store.subtasks(&child).await.unwrap().is_empty());
    assert!(scratch.store.subtasks(&loose).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_task_cannot_be_its_own_parent() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let task = add_task(&scratch.store, &workspace, "Backlog", "Alone", None, &admin).await;

    assert!(matches!(
        scratch.store.set_parent(&task, Some(&task)).await,
        Err(StoreError::Cycle)
    ));
    assert!(scratch.store.subtasks(&task).await.unwrap().is_empty());
}

#[tokio::test]
async fn parenting_and_promoting_are_each_one_write() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let parent = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Parent",
        None,
        &admin,
    )
    .await;
    let loose = add_task(&scratch.store, &workspace, "Review", "Loose", None, &admin).await;

    scratch
        .store
        .set_parent(&loose, Some(&parent))
        .await
        .unwrap();
    let children = scratch.store.subtasks(&parent).await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, loose);
    // It kept everything a task has: it did not move column to become one.
    assert_eq!(
        children[0].column_id,
        column_named(&scratch.store, &workspace, "Review").await
    );

    scratch.store.set_parent(&loose, None).await.unwrap();
    assert!(scratch.store.subtasks(&parent).await.unwrap().is_empty());
}

#[tokio::test]
async fn parenting_a_task_that_is_not_there_is_not_found() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let task = add_task(&scratch.store, &workspace, "Backlog", "Alone", None, &admin).await;

    assert!(matches!(
        scratch.store.set_parent(&task, Some("nope")).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        scratch.store.set_parent("nope", None).await,
        Err(StoreError::NotFound)
    ));
}

/// A parent in Backlog with one subtask, also in Backlog. Returns both ids.
async fn a_parent_and_one_subtask(
    store: &TursoStore,
    workspace: &str,
    admin: &str,
) -> (String, String) {
    let parent = add_task(
        store,
        workspace,
        "Backlog",
        "Ship the exporter",
        None,
        admin,
    )
    .await;
    let board = store.board(workspace).await.unwrap().unwrap();
    let column_id = column_named(store, workspace, "Backlog").await;
    let child = store
        .create_task(NewTask {
            clock_at: None,
            board_id: &board.id,
            column_id: &column_id,
            parent_id: Some(&parent),
            title: "Write the CSV writer",
            description: "",
            deadline: None,
            created_by: admin,
        })
        .await
        .unwrap()
        .row
        .id;
    (parent, child)
}

#[tokio::test]
async fn a_parent_does_not_finish_before_its_subtasks_do() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;

    let backlog = column_named(&scratch.store, &workspace, "Backlog").await;
    let done = column_named(&scratch.store, &workspace, "Done").await;
    let held = scratch
        .store
        .move_task(&parent, &backlog, &done, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(matches!(held, Moved::Held));

    // Nothing was written: not the column, not a crossing for a rule to read.
    let detail = load_detail(&scratch.store, &workspace, &parent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.column.id, backlog);
    assert!(detail.done_at.is_none());
    assert!(
        !detail
            .activity
            .iter()
            .any(|entry| matches!(entry.kind, izlek_core::detail::ActivityKind::Moved)),
        "a held move leaves no trail"
    );

    // The subtask finishing is what lets the parent through.
    moved_to(
        &scratch.store,
        &workspace,
        &child,
        "Backlog",
        "Done",
        &admin,
    )
    .await;
    let recorded = scratch
        .store
        .move_task(&parent, &backlog, &done, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(matches!(recorded, Moved::Recorded(_)));
    assert!(
        load_detail(&scratch.store, &workspace, &parent)
            .await
            .unwrap()
            .unwrap()
            .done_at
            .is_some()
    );
}

#[tokio::test]
async fn an_open_subtask_only_holds_the_done_column() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, _child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;

    // Every other column is a column like any other.
    moved_to(
        &scratch.store,
        &workspace,
        &parent,
        "Backlog",
        "In Progress",
        &admin,
    )
    .await;
    moved_to(
        &scratch.store,
        &workspace,
        &parent,
        "In Progress",
        "Review",
        &admin,
    )
    .await;
}

#[tokio::test]
async fn a_subtask_finishes_on_its_own_account() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (_parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;

    // The rule is about a parent's parts, not about being somebody's part.
    moved_to(
        &scratch.store,
        &workspace,
        &child,
        "Backlog",
        "Done",
        &admin,
    )
    .await;
}

#[tokio::test]
async fn a_deleted_subtask_stops_holding_its_parent() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;

    // The escape hatch is not an override button: a subtask nobody will do is
    // deleted, or promoted out of its parent.
    scratch
        .store
        .delete_task(&child, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    moved_to(
        &scratch.store,
        &workspace,
        &parent,
        "Backlog",
        "Done",
        &admin,
    )
    .await;
}

#[tokio::test]
async fn a_promoted_subtask_stops_holding_its_parent() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;

    scratch.store.set_parent(&child, None).await.unwrap();
    moved_to(
        &scratch.store,
        &workspace,
        &parent,
        "Backlog",
        "Done",
        &admin,
    )
    .await;
}

#[tokio::test]
async fn the_board_counts_subtasks_instead_of_carding_them() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;

    let board = load(&scratch.store, &workspace).await.unwrap().unwrap();
    let cards: Vec<&izlek_core::board::TaskCard> =
        board.columns.iter().flat_map(|c| &c.cards).collect();
    assert_eq!(cards.len(), 1, "the subtask took a card of its own");
    assert_eq!(cards[0].id, parent);
    assert_eq!(cards[0].subtask_total, 1);
    assert_eq!(cards[0].subtask_done, 0);
    assert_eq!(cards[0].subtask_label().as_deref(), Some("0/1"));
    assert!(cards[0].holds_on_subtasks());

    moved_to(
        &scratch.store,
        &workspace,
        &child,
        "Backlog",
        "Done",
        &admin,
    )
    .await;
    let board = load(&scratch.store, &workspace).await.unwrap().unwrap();
    let card = board
        .columns
        .iter()
        .flat_map(|c| &c.cards)
        .find(|card| card.id == parent)
        .unwrap();
    assert_eq!(card.subtask_label().as_deref(), Some("1/1"));
    assert!(!card.holds_on_subtasks());
}

#[tokio::test]
async fn a_card_with_no_parts_wears_no_chip() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    add_task(&scratch.store, &workspace, "Backlog", "Alone", None, &admin).await;

    let board = load(&scratch.store, &workspace).await.unwrap().unwrap();
    let card = board.columns.iter().flat_map(|c| &c.cards).next().unwrap();
    assert_eq!(card.subtask_label(), None);
    assert!(!card.holds_on_subtasks());
}

#[tokio::test]
async fn a_subtask_blocking_a_card_still_names_itself_on_it() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (_parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;
    let other = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Waiting",
        None,
        &admin,
    )
    .await;
    scratch
        .store
        .add_dependency(&other, &child, OffsetDateTime::now_utc())
        .await
        .unwrap();

    // The subtask has no card, but its key is real and the chip has to show
    // it: dropping the row would leave the blocked card looking free.
    let board = load(&scratch.store, &workspace).await.unwrap().unwrap();
    let waiting = board
        .columns
        .iter()
        .flat_map(|c| &c.cards)
        .find(|card| card.id == other)
        .unwrap();
    assert_eq!(waiting.blocked_by.len(), 1);
    assert!(waiting.is_blocked());
}

#[tokio::test]
async fn a_parent_and_its_own_part_are_not_linked_as_well() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;
    let now = OffsetDateTime::now_utc();

    assert!(matches!(
        scratch.store.add_dependency(&parent, &child, now).await,
        Err(StoreError::Cycle)
    ));
    assert!(matches!(
        scratch.store.add_dependency(&child, &parent, now).await,
        Err(StoreError::Cycle)
    ));
}

#[tokio::test]
async fn a_task_detail_carries_its_family_both_ways() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;
    scratch.store.assign_task(&child, &admin).await.unwrap();

    let whole = load_detail(&scratch.store, &workspace, &parent)
        .await
        .unwrap()
        .unwrap();
    assert!(whole.parent.is_none());
    assert_eq!(whole.subtasks.len(), 1);
    assert_eq!(whole.subtasks[0].id, child);
    assert_eq!(whole.subtasks[0].assignees.len(), 1);
    assert!(!whole.subtasks[0].is_done());

    let part = load_detail(&scratch.store, &workspace, &child)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(part.parent.as_ref().unwrap().id, parent);
    assert!(
        part.subtasks.is_empty(),
        "a subtask has no parts of its own"
    );
}

#[tokio::test]
async fn deleting_a_parent_says_what_it_takes_and_takes_it() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;

    let cost = scratch
        .store
        .deletion_cost(&parent)
        .await
        .unwrap()
        .expect("the parent is there to delete");
    assert_eq!(cost.subtask_count, 1);

    scratch
        .store
        .delete_task(&parent, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();

    // The part went with the whole, in the same write. A subtask that
    // outlived its parent would be unreachable.
    assert!(
        load_detail(&scratch.store, &workspace, &child)
            .await
            .unwrap()
            .is_none()
    );
    let board = load(&scratch.store, &workspace).await.unwrap().unwrap();
    assert_eq!(board.columns.iter().flat_map(|c| &c.cards).count(), 0);
}

#[tokio::test]
async fn deleting_a_subtask_leaves_its_parent_standing() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let (parent, child) = a_parent_and_one_subtask(&scratch.store, &workspace, &admin).await;

    scratch
        .store
        .delete_task(&child, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();

    let whole = load_detail(&scratch.store, &workspace, &parent)
        .await
        .unwrap()
        .unwrap();
    assert!(whole.subtasks.is_empty());
    let board = load(&scratch.store, &workspace).await.unwrap().unwrap();
    let card = board.columns.iter().flat_map(|c| &c.cards).next().unwrap();
    assert_eq!(card.subtask_label(), None);
}

#[tokio::test]
async fn the_first_account_owns_the_workspace() {
    let scratch = Scratch::open().await;
    assert!(scratch.store.workspace().await.unwrap().is_none());
    assert!(scratch.store.owner().await.unwrap().is_none());

    let (_, admin_id) = claim(&scratch.store).await;
    let owner = scratch.store.owner().await.unwrap().unwrap();
    assert_eq!(owner.id, admin_id);
    assert_eq!(owner.role, Role::Admin);
    assert!(owner.has_signed_in(), "the admin sets their own password");
}

#[tokio::test]
async fn a_second_claim_loses_and_changes_nothing() {
    let scratch = Scratch::open().await;
    claim(&scratch.store).await;

    let second = scratch
        .store
        .claim_workspace(
            "Someone else's",
            "mallory@elsewhere.example",
            "Mallory",
            &hash_password("tide-tables-1892").unwrap(),
        )
        .await;
    assert!(matches!(second, Err(StoreError::AlreadyClaimed)));

    // The loser must not have joined as anything at all.
    assert_eq!(
        scratch.store.workspace().await.unwrap().unwrap().name,
        "İzlek"
    );
    assert_eq!(scratch.store.count_users("").await.unwrap(), 0);
    let ws_id = scratch.store.workspace().await.unwrap().unwrap().id;
    assert_eq!(scratch.store.count_users(&ws_id).await.unwrap(), 1);
    assert!(
        scratch
            .store
            .user_by_email(&ws_id, "mallory@elsewhere.example")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_produce_exactly_one_admin() {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();
    let store = std::sync::Arc::new(TursoStore::open(&path).await.unwrap());

    let hash = hash_password("tide-tables-1892").unwrap();
    let mut claims = Vec::new();
    for i in 0..4 {
        let store = store.clone();
        let hash = hash.clone();
        claims.push(tokio::spawn(async move {
            store
                .claim_workspace(
                    "İzlek",
                    &format!("claimant{i}@izlek.sh"),
                    &format!("Claimant {i}"),
                    &hash,
                )
                .await
                .map(|(_, admin)| admin.email)
        }));
    }

    let mut winners = Vec::new();
    for claim in claims {
        match claim.await.unwrap() {
            Ok(email) => winners.push(email),
            Err(StoreError::AlreadyClaimed) => {}
            Err(other) => panic!("unexpected claim failure: {other}"),
        }
    }
    assert_eq!(winners.len(), 1, "exactly one claim wins: {winners:?}");

    let ws_id = store.workspace().await.unwrap().unwrap().id;
    assert_eq!(
        store.count_users(&ws_id).await.unwrap(),
        1,
        "no half-written losers"
    );
    let owner = store.owner().await.unwrap().unwrap();
    assert_eq!(owner.email, winners[0]);

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The store's connection is not concurrent-safe on its own (turso's
/// `TursoError::Misuse("concurrent use forbidden")` fires the moment two
/// queries overlap on it); this drives reads and writes against one shared
/// [`TursoStore`] from many tasks at once and expects every one of them to
/// come back `Ok`, never that error.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn many_concurrent_callers_never_see_concurrent_use_forbidden() {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();
    let store = std::sync::Arc::new(TursoStore::open(&path).await.unwrap());
    let (workspace_id, _) = claim(&store).await;

    let mut tasks = Vec::new();
    for i in 0..16 {
        let store = store.clone();
        let workspace_id = workspace_id.clone();
        tasks.push(tokio::spawn(async move {
            if i % 2 == 0 {
                // A write: inserts into auth_attempt on the shared connection.
                store
                    .record_auth_attempt(&format!("bucket-{i}"), OffsetDateTime::now_utc())
                    .await
                    .map(|_| ())
            } else {
                // A multi-row read that steps a statement in a loop.
                store.users(&workspace_id).await.map(|_| ())
            }
        }));
    }

    for task in tasks {
        task.await.unwrap().expect(
            "concurrent store access must never surface Misuse(\"concurrent use forbidden\")",
        );
    }

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workspace_defaults_match_the_settings_screen() {
    let scratch = Scratch::open().await;
    let (ws, _) = scratch
        .store
        .claim_workspace(
            "İzlek",
            "ada@izlek.sh",
            "Ada",
            &hash_password("tide-tables-1892").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ws.attachment_limit_bytes, 25 * 1024 * 1024);
    assert_eq!(ws.photo_limit_bytes, 2 * 1024 * 1024);
    assert!(
        ws.allowed_file_types.is_empty(),
        "every type until narrowed"
    );
    assert_eq!(ws.mail_batch_minutes, 5);
    assert_eq!(ws.reminder_minutes, 15, "a workspace starts with reminders on");
}

/// An admin edits the sender from Settings, so the record carries host, port,
/// username and from-address. It never carries the password: that field does
/// not exist on the struct, and what a screen gets is the fact that one is set.
#[tokio::test]
async fn the_workspace_record_carries_the_sender_but_never_its_password() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let fresh = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(fresh.smtp_host, None, "a new workspace has no sender");
    assert!(!fresh.smtp_password_set);

    scratch
        .store
        .set_sender(
            &ws_id,
            NewSender {
                host: "smtp.fastmail.com".into(),
                port: 587,
                username: "izlek".into(),
                password: Some("a-very-secret-string".into()),
                from_name: "İzlek".into(),
                from_address: "izlek@izlek.sh".into(),
            },
        )
        .await
        .unwrap();

    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(ws.smtp_host.as_deref(), Some("smtp.fastmail.com"));
    assert_eq!(ws.smtp_port, Some(587));
    assert_eq!(ws.smtp_username.as_deref(), Some("izlek"));
    assert_eq!(ws.smtp_from_name.as_deref(), Some("İzlek"));
    assert_eq!(ws.smtp_from_address.as_deref(), Some("izlek@izlek.sh"));
    assert!(ws.smtp_password_set, "the screen must be able to say 'set'");

    let serialised = serde_json::to_string(&ws).unwrap();
    assert!(
        !serialised.contains("a-very-secret-string"),
        "the password rode along in the workspace record: {serialised}"
    );
}

/// Changing the port must not blank the password. The field is write-only, so
/// the screen has nothing to send back for it, and a save that took the empty
/// field literally would silently stop the workspace sending mail.
#[tokio::test]
async fn a_save_with_no_password_typed_keeps_the_stored_one() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let mut sender = NewSender {
        host: "smtp.fastmail.com".into(),
        port: 587,
        username: "izlek".into(),
        password: Some("keep-me".into()),
        from_name: "İzlek".into(),
        from_address: "izlek@izlek.sh".into(),
    };
    scratch
        .store
        .set_sender(&ws_id, sender.clone())
        .await
        .unwrap();

    sender.port = 465;
    sender.password = None;
    scratch.store.set_sender(&ws_id, sender).await.unwrap();

    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(ws.smtp_port, Some(465), "the edit did not land");
    assert!(ws.smtp_password_set, "the password was blanked by an edit");
    assert_eq!(
        scratch
            .store
            .smtp_password(&ws_id)
            .await
            .unwrap()
            .as_deref(),
        Some("keep-me")
    );
}

/// And a password that is typed replaces the old one, which is the other half:
/// rotation has to work without a shell on the box.
#[tokio::test]
async fn a_typed_password_replaces_the_stored_one() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let mut sender = NewSender {
        host: "smtp.fastmail.com".into(),
        port: 587,
        username: "izlek".into(),
        password: Some("the-old-one".into()),
        from_name: "İzlek".into(),
        from_address: "izlek@izlek.sh".into(),
    };
    scratch
        .store
        .set_sender(&ws_id, sender.clone())
        .await
        .unwrap();
    sender.password = Some("the-new-one".into());
    scratch.store.set_sender(&ws_id, sender).await.unwrap();

    assert_eq!(
        scratch
            .store
            .smtp_password(&ws_id)
            .await
            .unwrap()
            .as_deref(),
        Some("the-new-one")
    );
}

/// The point of encrypting the column: what actually sits on disk is not the
/// password. A raw read of the row must not turn it up even in ciphertext
/// form recognisable as the plaintext.
#[tokio::test]
async fn the_stored_password_is_not_the_plaintext_on_disk() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    scratch
        .store
        .set_sender(
            &ws_id,
            NewSender {
                host: "smtp.fastmail.com".into(),
                port: 587,
                username: "izlek".into(),
                password: Some("a-very-secret-string".into()),
                from_name: "İzlek".into(),
                from_address: "izlek@izlek.sh".into(),
            },
        )
        .await
        .unwrap();

    let conn = raw_conn(&scratch).await;
    let mut rows = conn
        .query(
            "SELECT smtp_password FROM workspace WHERE id = ?1",
            turso::params![ws_id.clone()],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let column: String = row.get(0).unwrap();
    assert!(
        !column.contains("a-very-secret-string"),
        "the plaintext is sitting in the column: {column}"
    );
    assert!(
        column.starts_with("v1:"),
        "expected the sealed envelope, got: {column}"
    );

    // And the read path still gets the real password back.
    assert_eq!(
        scratch
            .store
            .smtp_password(&ws_id)
            .await
            .unwrap()
            .as_deref(),
        Some("a-very-secret-string")
    );
}

/// A backup restored without its sibling `izlek.key`, or a key file damaged
/// in place, must not crash mail sending or the settings screen — it has to
/// degrade to "no password set", exactly like a workspace that never had a
/// sender, so the admin can heal it by retyping the password once.
#[tokio::test]
async fn a_password_that_will_not_decrypt_reads_back_as_none() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    scratch
        .store
        .set_sender(
            &ws_id,
            NewSender {
                host: "smtp.fastmail.com".into(),
                port: 587,
                username: "izlek".into(),
                password: Some("a-very-secret-string".into()),
                from_name: "İzlek".into(),
                from_address: "izlek@izlek.sh".into(),
            },
        )
        .await
        .unwrap();

    // Simulate a lost key / corrupted ciphertext: overwrite the column with
    // garbage that still carries the envelope prefix, the way a truncated or
    // bit-flipped backup would.
    let conn = raw_conn(&scratch).await;
    conn.execute(
        "UPDATE workspace SET smtp_password = 'v1:not-actually-sealed' WHERE id = ?1",
        turso::params![ws_id.clone()],
    )
    .await
    .unwrap();

    // Reading degrades to None, not an error.
    assert_eq!(scratch.store.smtp_password(&ws_id).await.unwrap(), None);

    // The screen still says "set" — a value is present — but retyping heals it.
    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert!(ws.smtp_password_set);

    scratch
        .store
        .set_sender(
            &ws_id,
            NewSender {
                host: "smtp.fastmail.com".into(),
                port: 587,
                username: "izlek".into(),
                password: Some("a-fresh-password".into()),
                from_name: "İzlek".into(),
                from_address: "izlek@izlek.sh".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        scratch
            .store
            .smtp_password(&ws_id)
            .await
            .unwrap()
            .as_deref(),
        Some("a-fresh-password"),
        "retyping the password heals a store that could not decrypt it"
    );
}

/// `TursoStore::open` closes the world-readable window: the database file and
/// its key file come out at 0600, not whatever umask the process inherited.
#[tokio::test]
#[cfg(unix)]
async fn the_database_and_key_file_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::open().await;
    let db_path = scratch.dir.join("izlek.db");
    let key_path = scratch.dir.join("izlek.key");
    for path in [&db_path, &key_path] {
        let mode = std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} is not owner-only: {mode:o}",
            path.display()
        );
    }
}

#[tokio::test]
async fn limits_round_trip_including_the_file_type_list() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let types = vec!["png".to_string(), "pdf".to_string()];
    scratch
        .store
        .set_limits(&ws_id, 10 * 1024 * 1024, 512 * 1024, &types, 5, 15)
        .await
        .unwrap();
    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(ws.attachment_limit_bytes, 10 * 1024 * 1024);
    assert_eq!(ws.photo_limit_bytes, 512 * 1024);
    assert_eq!(ws.allowed_file_types, types);
    assert_eq!(ws.mail_batch_minutes, 5);
    assert_eq!(ws.reminder_minutes, 15, "the reminder lead rides the same save");
}

#[tokio::test]
async fn an_invited_member_has_no_password_until_they_choose_one() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let member = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id.clone(),
            email: "grace@izlek.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
            invited_by: None,
        })
        .await
        .unwrap();
    assert!(member.password_hash.is_none());
    assert!(!member.has_signed_in());

    scratch
        .store
        .set_password_hash(&member.id, "$argon2id$fake")
        .await
        .unwrap();
    let member = scratch.store.user(&member.id).await.unwrap().unwrap();
    assert!(member.has_signed_in());
}

#[tokio::test]
async fn addresses_are_unique_and_case_insensitive() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let dup = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id.clone(),
            email: "  ADA@Izlek.sh ".into(),
            display_name: "Ada again".into(),
            role: Role::Member,
            invited_by: None,
        })
        .await;
    assert!(matches!(dup, Err(StoreError::Conflict("account"))));
    assert!(
        scratch
            .store
            .user_by_email(&ws_id, "Ada@IZLEK.sh")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn an_unknown_address_is_a_plain_none() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    // The sign-in surface builds its uniform response on this; the store must
    // not distinguish "no such account" from anything else by erroring.
    assert!(
        scratch
            .store
            .user_by_email(&ws_id, "nobody@izlek.sh")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn members_list_and_count_for_the_admin_screen() {
    let (scratch, ws_id, admin_id) = workspace_with_admin().await;
    for (email, name, role) in [
        ("grace@izlek.sh", "Grace", Role::Member),
        ("linus@izlek.sh", "Linus", Role::Viewer),
    ] {
        scratch
            .store
            .create_user(NewUser {
                workspace_id: ws_id.clone(),
                email: email.into(),
                display_name: name.into(),
                role,
                invited_by: None,
            })
            .await
            .unwrap();
    }
    assert_eq!(scratch.store.count_users(&ws_id).await.unwrap(), 3);
    let users = scratch.store.users(&ws_id).await.unwrap();
    assert_eq!(users[0].id, admin_id);
    assert_eq!(users.iter().filter(|u| u.role == Role::Viewer).count(), 1);
}

#[tokio::test]
async fn profile_and_role_updates_stick() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let user = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id,
            email: "grace@izlek.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
            invited_by: None,
        })
        .await
        .unwrap();

    scratch
        .store
        .set_profile(&user.id, "Grace H.")
        .await
        .unwrap();
    scratch
        .store
        .set_photo(&user.id, b"grace-bytes", "image/png")
        .await
        .unwrap();
    scratch
        .store
        .set_role(&user.id, Role::Viewer)
        .await
        .unwrap();
    let at = OffsetDateTime::now_utc();
    scratch.store.mark_signed_in(&user.id, at).await.unwrap();

    let photo = scratch.store.photo(&user.id).await.unwrap().unwrap();
    assert_eq!(photo.0, b"grace-bytes".to_vec());
    assert_eq!(photo.1, "image/png");
    let user = scratch.store.user(&user.id).await.unwrap().unwrap();
    assert_eq!(user.display_name, "Grace H.");
    assert!(user.has_photo);
    assert_eq!(user.role, Role::Viewer);
    // Stored as RFC 3339 text, so equality holds to the second.
    assert_eq!(
        user.last_signed_in_at.unwrap().unix_timestamp(),
        at.unix_timestamp()
    );

    // Clearing the photo is a real update, not a no-op.
    scratch.store.clear_photo(&user.id).await.unwrap();
    let user = scratch.store.user(&user.id).await.unwrap().unwrap();
    assert!(!user.has_photo);
    assert!(scratch.store.photo(&user.id).await.unwrap().is_none());
}

#[tokio::test]
async fn email_change_persists_and_refuses_a_taken_address() {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_str().unwrap().to_string();

    let user_id = {
        let store = TursoStore::open(&path).await.unwrap();
        let (ws_id, _admin_id) = claim(&store).await;
        let user_id = member(&store, &ws_id, "grace@izlek.sh", "Grace").await;
        store
            .set_email(&user_id, &ws_id, "Grace.New@Izlek.sh")
            .await
            .unwrap();
        // Case-folded the same way sign-in matches an address.
        assert!(
            store
                .user_by_email(&ws_id, "grace.new@izlek.sh")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .user_by_email(&ws_id, "grace@izlek.sh")
                .await
                .unwrap()
                .is_none()
        );

        // Taken by the admin already claimed above.
        let err = store
            .set_email(&user_id, &ws_id, "ada@izlek.sh")
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict("account")));
        user_id
    };

    // Persists across a reopen, and the refused attempt did not stick.
    let store = TursoStore::open(&path).await.unwrap();
    let user = store.user(&user_id).await.unwrap().unwrap();
    assert_eq!(user.email, "grace.new@izlek.sh");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn display_preferences_default_and_persist_across_reopen() {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();

    let store = TursoStore::open(&path).await.unwrap();
    let (_, admin_id) = claim(&store).await;

    let admin = store.user(&admin_id).await.unwrap().unwrap();
    assert_eq!(admin.timezone, "UTC");
    assert_eq!(admin.theme, "light");
    assert_eq!(admin.language, "en");
    assert_eq!(admin.ui, "instrument");

    store
        .set_preferences(&admin_id, "Europe/Istanbul", "dark", "tr", "ledger")
        .await
        .unwrap();
    drop(store);

    let reopened = TursoStore::open(&path).await.unwrap();
    let admin = reopened.user(&admin_id).await.unwrap().unwrap();
    assert_eq!(admin.timezone, "Europe/Istanbul");
    assert_eq!(admin.theme, "dark");
    assert_eq!(admin.language, "tr");
    assert_eq!(admin.ui, "ledger");

    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn updates_to_a_missing_user_are_not_found() {
    let scratch = Scratch::open().await;
    let missing = Ulid::new().to_string();
    assert!(matches!(
        scratch.store.set_password_hash(&missing, "x").await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        scratch.store.set_role(&missing, Role::Member).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_signin_link_stores_only_the_hash_and_is_used_once() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let user = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id,
            email: "grace@izlek.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
            invited_by: None,
        })
        .await
        .unwrap();

    let now = OffsetDateTime::now_utc();
    let link = scratch
        .store
        .create_signin_link(&user.id, "hash-of-the-token", now + Duration::days(7))
        .await
        .unwrap();
    assert!(link.is_usable(now));

    // Lookup is by hash: the plaintext never reaches the database.
    assert!(
        scratch
            .store
            .signin_link_by_hash("hash-of-the-token")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        scratch
            .store
            .signin_link_by_hash("some-other-hash")
            .await
            .unwrap()
            .is_none()
    );

    assert!(
        scratch
            .store
            .consume_signin_link(&link.id, now)
            .await
            .unwrap()
    );
    let used = scratch
        .store
        .signin_link_by_hash("hash-of-the-token")
        .await
        .unwrap()
        .unwrap();
    assert!(!used.is_usable(now));
    // A second use finds nothing left to consume.
    assert!(
        !scratch
            .store
            .consume_signin_link(&link.id, now)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_expired_link_is_still_a_live_account() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let user = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id,
            email: "grace@izlek.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
            invited_by: None,
        })
        .await
        .unwrap();

    let now = OffsetDateTime::now_utc();
    let stale = scratch
        .store
        .create_signin_link(&user.id, "stale-hash", now - Duration::hours(1))
        .await
        .unwrap();
    assert!(!stale.is_usable(now));

    // Resending opens the same account rather than making a new one.
    let fresh = scratch
        .store
        .create_signin_link(&user.id, "fresh-hash", now + Duration::days(7))
        .await
        .unwrap();
    assert_eq!(fresh.user_id, stale.user_id);
    assert!(fresh.is_usable(now));
    assert!(scratch.store.user(&user.id).await.unwrap().is_some());
}

#[tokio::test]
async fn the_store_is_usable_behind_a_trait_object() {
    let scratch = Scratch::open().await;
    let store: &dyn Store = &scratch.store;
    store
        .claim_workspace("İzlek", "ada@izlek.sh", "Ada", "$argon2id$fake")
        .await
        .unwrap();
    assert!(store.workspace().await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prefetched_link_is_consumed_exactly_once() {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();
    let store = std::sync::Arc::new(TursoStore::open(&path).await.unwrap());
    let (ws_id, _) = claim(&store).await;
    let user_id = member(&store, &ws_id, "grace@izlek.sh", "Grace").await;

    let token = Token::mint();
    let now = OffsetDateTime::now_utc();
    let link = store
        .create_signin_link(&user_id, &token.hash(), now + Duration::days(7))
        .await
        .unwrap();

    // The mail client prefetches while the person clicks.
    let mut redemptions = Vec::new();
    for _ in 0..4 {
        let store = store.clone();
        let id = link.id.clone();
        redemptions.push(tokio::spawn(async move {
            store.consume_signin_link(&id, now).await.unwrap()
        }));
    }
    let mut winners = 0;
    for redemption in redemptions {
        if redemption.await.unwrap() {
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "exactly one redemption may win");

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_session_lives_until_it_expires_or_is_revoked() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let user_id = member(&scratch.store, &ws_id, "grace@izlek.sh", "Grace").await;

    let token = Token::mint();
    let now = OffsetDateTime::now_utc();
    let session = scratch
        .store
        .create_session(&user_id, &token.hash(), now + Duration::days(14))
        .await
        .unwrap();
    assert!(session.is_live(now));

    // The cookie value is the only way in, and it is not what is stored.
    let found = scratch
        .store
        .session_by_hash(&token.hash())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, session.id);
    assert!(
        scratch
            .store
            .session_by_hash(token.expose())
            .await
            .unwrap()
            .is_none(),
        "the plaintext must not be stored"
    );
    assert_eq!(
        scratch.store.session_token_hash(&session.id).await.unwrap(),
        Some(token.hash())
    );

    scratch
        .store
        .revoke_session(&session.id, now)
        .await
        .unwrap();
    let revoked = scratch
        .store
        .session_by_hash(&token.hash())
        .await
        .unwrap()
        .unwrap();
    assert!(!revoked.is_live(now), "a revoked session is dead at once");
    // Revoking twice is not an error the caller has to think about.
    assert!(matches!(
        scratch.store.revoke_session(&session.id, now).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn an_expired_session_is_not_live() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let user_id = member(&scratch.store, &ws_id, "grace@izlek.sh", "Grace").await;
    let now = OffsetDateTime::now_utc();
    let session = scratch
        .store
        .create_session(&user_id, &Token::mint().hash(), now - Duration::minutes(1))
        .await
        .unwrap();
    assert!(!session.is_live(now));
}

#[tokio::test]
async fn changing_a_password_can_sign_out_every_other_device() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let user_id = member(&scratch.store, &ws_id, "grace@izlek.sh", "Grace").await;
    let other_id = member(&scratch.store, &ws_id, "linus@izlek.sh", "Linus").await;
    let now = OffsetDateTime::now_utc();

    let mut hashes = Vec::new();
    for _ in 0..3 {
        let token = Token::mint();
        scratch
            .store
            .create_session(&user_id, &token.hash(), now + Duration::days(14))
            .await
            .unwrap();
        hashes.push(token.hash());
    }
    let bystander = Token::mint();
    scratch
        .store
        .create_session(&other_id, &bystander.hash(), now + Duration::days(14))
        .await
        .unwrap();

    let revoked = scratch
        .store
        .revoke_sessions_for_user(&user_id, now)
        .await
        .unwrap();
    assert_eq!(revoked, 3);
    for hash in &hashes {
        let session = scratch.store.session_by_hash(hash).await.unwrap().unwrap();
        assert!(!session.is_live(now));
    }
    // Someone else's browser is untouched.
    let bystander = scratch
        .store
        .session_by_hash(&bystander.hash())
        .await
        .unwrap()
        .unwrap();
    assert!(bystander.is_live(now));
}

#[tokio::test]
async fn auth_attempts_are_counted_over_a_window() {
    let scratch = Scratch::open().await;
    let now = OffsetDateTime::now_utc();
    let window = now - Duration::minutes(15);

    for minutes_ago in [30, 20, 10, 5, 1] {
        scratch
            .store
            .record_auth_attempt("grace@izlek.sh", now - Duration::minutes(minutes_ago))
            .await
            .unwrap();
    }
    scratch
        .store
        .record_auth_attempt("198.51.100.7", now)
        .await
        .unwrap();

    // Only the three inside the window count, and buckets do not bleed.
    assert_eq!(
        scratch
            .store
            .count_auth_attempts("grace@izlek.sh", window)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        scratch
            .store
            .count_auth_attempts("198.51.100.7", window)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        scratch
            .store
            .count_auth_attempts("nobody@izlek.sh", window)
            .await
            .unwrap(),
        0
    );

    // A success clears the bucket that succeeded, and only that one.
    scratch
        .store
        .clear_auth_attempts("grace@izlek.sh")
        .await
        .unwrap();
    assert_eq!(
        scratch
            .store
            .count_auth_attempts("grace@izlek.sh", window)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        scratch
            .store
            .count_auth_attempts("198.51.100.7", window)
            .await
            .unwrap(),
        1
    );

    // Pruning drops what is out of every window.
    scratch
        .store
        .record_auth_attempt("grace@izlek.sh", now - Duration::hours(2))
        .await
        .unwrap();
    let pruned = scratch
        .store
        .prune_auth_attempts(now - Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(pruned, 1);
}

// ---------------------------------------------------------------------------
// Account flows.
// ---------------------------------------------------------------------------

use izlek_core::accounts::{AccountError, Accounts, RATE_LIMIT, SIGNIN_LINK_LIFETIME};
use izlek_core::auth::PasswordProblem;
use std::sync::Arc;

/// An accounts service over a throwaway database. The directory is kept alive
/// by the returned guard.
async fn accounts() -> (Scratch, Accounts) {
    let scratch = Scratch::open().await;
    let store = TursoStore::open(scratch.dir.join("izlek.db").to_str().unwrap())
        .await
        .unwrap();
    let accounts = Accounts::new(Arc::new(store) as Arc<dyn Store>, "https://izlek.sh");
    (scratch, accounts)
}

async fn claimed() -> (Scratch, Accounts, User) {
    let (scratch, accounts) = accounts().await;
    let (_, signed_in) = accounts
        .claim_workspace("İzlek", "ada@izlek.sh", "Ada", "tide-tables-1892")
        .await
        .unwrap();
    (scratch, accounts, signed_in.user)
}

#[tokio::test]
async fn claiming_makes_an_admin_and_signs_them_in() {
    let (_scratch, accounts) = accounts().await;
    let (workspace, signed_in) = accounts
        .claim_workspace("İzlek", "ada@izlek.sh", "Ada", "tide-tables-1892")
        .await
        .unwrap();
    assert_eq!(workspace.name, "İzlek");
    assert_eq!(signed_in.user.role, Role::Admin);
    assert!(
        signed_in.user.last_signed_in_at.is_some(),
        "the claim is their first arrival"
    );

    // The cookie value works and is not what is stored.
    let who = accounts
        .authenticate(signed_in.session_token.expose())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(who.id, signed_in.user.id);
    assert!(
        accounts
            .authenticate(&signed_in.session.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_claim_screen_enforces_its_own_password_rules() {
    let (_scratch, accounts) = accounts().await;
    let too_short = accounts
        .claim_workspace("İzlek", "ada@izlek.sh", "Ada", "short")
        .await;
    assert!(matches!(
        too_short,
        Err(AccountError::Password(PasswordProblem::TooShort))
    ));
    // A rejected password must not have claimed anything.
    assert!(accounts.store().workspace().await.unwrap().is_none());
}

#[tokio::test]
async fn a_second_claim_is_refused() {
    let (_scratch, accounts, _admin) = claimed().await;
    let second = accounts
        .claim_workspace(
            "Theirs",
            "mallory@elsewhere.example",
            "Mallory",
            "tide-tables-1892",
        )
        .await;
    assert!(matches!(second, Err(AccountError::AlreadyClaimed)));
}

#[tokio::test]
async fn an_invited_member_chooses_their_own_password() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();
    assert!(!invitation.user.has_signed_in(), "no password yet");
    assert!(
        invitation.expires_at
            > OffsetDateTime::now_utc() + SIGNIN_LINK_LIFETIME - Duration::minutes(1)
    );

    // The admin cannot sign in as them in the meantime.
    let as_them = accounts
        .sign_in("grace@izlek.sh", "tide-tables-1892", "198.51.100.7")
        .await;
    assert!(matches!(as_them, Err(AccountError::Rejected)));

    let signed_in = accounts
        .redeem_signin_link(
            invitation.token.expose(),
            "sextant-and-chart",
            "198.51.100.7",
        )
        .await
        .unwrap();
    assert_eq!(signed_in.user.id, invitation.user.id);
    assert!(signed_in.user.has_signed_in());
    assert!(
        signed_in.user.last_signed_in_at.is_some(),
        "redeeming the link is their first arrival"
    );

    // And now the password they chose works, and only that one.
    accounts
        .sign_in("grace@izlek.sh", "sextant-and-chart", "198.51.100.7")
        .await
        .unwrap();
}

#[tokio::test]
async fn adding_a_member_queues_the_mail_that_carries_the_link() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();

    let queue = accounts
        .store()
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let queued = queue
        .iter()
        .find(|s| s.recipient == "grace@izlek.sh")
        .expect("the invite mail is on the outbox");
    assert_eq!(queued.kind, SendKind::Invite);
    let body = queued.body.as_deref().unwrap();
    let link = format!("https://izlek.sh/join/{}", invitation.token.expose());
    assert!(body.contains(&link), "body was: {body}");
}

#[tokio::test]
async fn only_an_admin_may_invite() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();
    let member = accounts
        .redeem_signin_link(
            invitation.token.expose(),
            "sextant-and-chart",
            "198.51.100.7",
        )
        .await
        .unwrap()
        .user;

    // Server-side, not merely hidden: a member calling the flow is refused.
    let attempt = accounts
        .invite(&member, "linus@izlek.sh", "Linus", Role::Member)
        .await;
    assert!(matches!(attempt, Err(AccountError::Forbidden)));

    let mut viewer = member.clone();
    viewer.role = Role::Viewer;
    assert!(matches!(
        accounts
            .invite(&viewer, "linus@izlek.sh", "Linus", Role::Viewer)
            .await,
        Err(AccountError::Forbidden)
    ));
}

#[tokio::test]
async fn an_admin_may_change_a_members_role() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();

    accounts
        .set_role(&admin, &invitation.user.id, Role::Viewer)
        .await
        .unwrap();

    let reloaded = accounts
        .store()
        .user(&invitation.user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.role, Role::Viewer);
}

#[tokio::test]
async fn the_owner_may_not_have_their_role_changed_even_by_another_admin() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Admin)
        .await
        .unwrap();
    let other_admin = accounts
        .redeem_signin_link(
            invitation.token.expose(),
            "sextant-and-chart",
            "198.51.100.7",
        )
        .await
        .unwrap()
        .user;

    let attempt = accounts
        .set_role(&other_admin, &admin.id, Role::Member)
        .await;
    assert!(matches!(attempt, Err(AccountError::Forbidden)));

    let reloaded = accounts.store().user(&admin.id).await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Admin);
}

#[tokio::test]
async fn nobody_may_change_their_own_role() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();
    let member = accounts
        .redeem_signin_link(
            invitation.token.expose(),
            "sextant-and-chart",
            "198.51.100.7",
        )
        .await
        .unwrap()
        .user;

    let attempt = accounts.set_role(&member, &member.id, Role::Admin).await;
    assert!(matches!(attempt, Err(AccountError::Forbidden)));
}

#[tokio::test]
async fn a_link_works_once_and_a_wrong_one_never_does() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();

    accounts
        .redeem_signin_link(
            invitation.token.expose(),
            "sextant-and-chart",
            "198.51.100.7",
        )
        .await
        .unwrap();
    let again = accounts
        .redeem_signin_link(
            invitation.token.expose(),
            "another-password",
            "198.51.100.7",
        )
        .await;
    assert!(matches!(again, Err(AccountError::Rejected)));

    let invented = accounts
        .redeem_signin_link(&"0".repeat(32), "another-password", "198.51.100.7")
        .await;
    assert!(matches!(invented, Err(AccountError::Rejected)));

    // The password they actually set is untouched by either failure.
    accounts
        .sign_in("grace@izlek.sh", "sextant-and-chart", "198.51.100.7")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_rejected_password_does_not_burn_the_invitation() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();

    assert!(matches!(
        accounts
            .redeem_signin_link(invitation.token.expose(), "grace!!", "198.51.100.7")
            .await,
        Err(AccountError::Password(PasswordProblem::TooShort))
    ));
    assert!(matches!(
        accounts
            .redeem_signin_link(
                invitation.token.expose(),
                "grace-hopper-1906",
                "198.51.100.7"
            )
            .await,
        Err(AccountError::Password(PasswordProblem::LooksLikeYou))
    ));
    // Still redeemable with a password that passes.
    accounts
        .redeem_signin_link(
            invitation.token.expose(),
            "sextant-and-chart",
            "198.51.100.7",
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn an_expired_link_is_refused_and_resending_opens_the_same_account() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();

    // Age the link past its expiry by writing a fresh one with a past date.
    let stale = Token::mint();
    accounts
        .store()
        .create_signin_link(
            &invitation.user.id,
            &stale.hash(),
            OffsetDateTime::now_utc() - Duration::minutes(1),
        )
        .await
        .unwrap();
    assert!(matches!(
        accounts
            .redeem_signin_link(stale.expose(), "sextant-and-chart", "198.51.100.7")
            .await,
        Err(AccountError::Rejected)
    ));

    let resent = accounts
        .resend_invitation(&admin, &invitation.user.id)
        .await
        .unwrap();
    assert_eq!(resent.user.id, invitation.user.id, "same account, new link");
    let signed_in = accounts
        .redeem_signin_link(resent.token.expose(), "sextant-and-chart", "198.51.100.7")
        .await
        .unwrap();
    assert_eq!(signed_in.user.id, invitation.user.id);
}

#[tokio::test]
async fn an_unknown_address_and_a_wrong_password_are_indistinguishable() {
    let (_scratch, accounts, _admin) = claimed().await;
    let unknown = accounts
        .sign_in("nobody@izlek.sh", "tide-tables-1892", "198.51.100.7")
        .await;
    let wrong = accounts
        .sign_in("ada@izlek.sh", "not-her-password", "198.51.100.8")
        .await;
    assert!(matches!(unknown, Err(AccountError::Rejected)));
    assert!(matches!(wrong, Err(AccountError::Rejected)));
    assert_eq!(
        unknown.unwrap_err().to_string(),
        wrong.unwrap_err().to_string(),
        "the wording must not distinguish them either"
    );
}

#[tokio::test]
async fn the_miss_path_costs_what_the_hit_path_costs() {
    let (_scratch, accounts, _admin) = claimed().await;
    // Not a timing assertion — those are flaky. This asserts the work happens:
    // a miss takes at least as long as one Argon2 verify at our parameters,
    // which a skipped verify could not.
    let baseline = std::time::Instant::now();
    let _ = accounts
        .sign_in("ada@izlek.sh", "not-her-password", "198.51.100.7")
        .await;
    let hit_cost = baseline.elapsed();

    let started = std::time::Instant::now();
    let _ = accounts
        .sign_in("nobody@izlek.sh", "not-a-password", "198.51.100.8")
        .await;
    let miss_cost = started.elapsed();

    assert!(
        miss_cost * 4 > hit_cost,
        "miss {miss_cost:?} is suspiciously cheaper than hit {hit_cost:?}"
    );
}

#[tokio::test]
async fn an_invited_account_that_has_no_password_cannot_be_signed_into() {
    let (_scratch, accounts, admin) = claimed().await;
    accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();
    // Not "set your password first" — that would confirm the address exists.
    assert!(matches!(
        accounts.sign_in("grace@izlek.sh", "", "198.51.100.7").await,
        Err(AccountError::Rejected)
    ));
}

#[tokio::test]
async fn sign_in_attempts_are_rate_limited_per_address() {
    let (_scratch, accounts, _admin) = claimed().await;
    for _ in 0..RATE_LIMIT {
        let _ = accounts
            .sign_in("ada@izlek.sh", "wrong", "198.51.100.7")
            .await;
    }
    // The next attempt is refused before any Argon2 work happens.
    assert!(matches!(
        accounts
            .sign_in("ada@izlek.sh", "wrong", "203.0.113.9")
            .await,
        Err(AccountError::RateLimited)
    ));
    // A different address from a fresh client is unaffected.
    assert!(matches!(
        accounts
            .sign_in("someone@izlek.sh", "wrong", "203.0.113.9")
            .await,
        Err(AccountError::Rejected)
    ));
}

/// The lockout question, decided: an address bucket that refuses before the
/// verify lets anyone who knows a colleague's address keep them out with ten
/// wrong guesses every fifteen minutes, from anywhere, with no account of their
/// own. So the address bucket counts but never refuses a correct password.
#[tokio::test]
async fn a_flooded_address_never_locks_the_owner_out() {
    let (_scratch, accounts, _admin) = claimed().await;
    // Each guess from its own client, so only the address bucket fills.
    for i in 0..(RATE_LIMIT + 5) {
        let _ = accounts
            .sign_in("ada@izlek.sh", "wrong", &format!("203.0.113.{i}"))
            .await;
    }
    // The owner, from a client of their own, still gets in.
    accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
}

/// The client bucket is the one that caps the Argon2 work, and it does refuse.
#[tokio::test]
async fn sign_in_attempts_are_rate_limited_per_client() {
    let (_scratch, accounts, _admin) = claimed().await;
    for i in 0..RATE_LIMIT {
        let _ = accounts
            .sign_in(&format!("nobody{i}@izlek.sh"), "wrong", "203.0.113.9")
            .await;
    }
    // Refused before any Argon2 work, whatever address it asks about.
    assert!(matches!(
        accounts
            .sign_in("ada@izlek.sh", "tide-tables-1892", "203.0.113.9")
            .await,
        Err(AccountError::RateLimited)
    ));
}

/// `/join/<token>` runs a full Argon2 hash on every miss, so the guess rate has
/// to be capped on the client — a bucket keyed on the presented token would be
/// fresh for every guess and would never fire.
#[tokio::test]
async fn link_redemption_is_rate_limited_per_client() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();
    for i in 0..RATE_LIMIT {
        let bogus = format!("{i:032x}");
        assert!(matches!(
            accounts
                .redeem_signin_link(&bogus, "sextant-and-chart", "203.0.113.9")
                .await,
            Err(AccountError::Rejected)
        ));
    }
    // Even the real link is refused now, from that client.
    assert!(matches!(
        accounts
            .redeem_signin_link(
                invitation.token.expose(),
                "sextant-and-chart",
                "203.0.113.9"
            )
            .await,
        Err(AccountError::RateLimited)
    ));
    // And the person on another machine is unaffected.
    accounts
        .redeem_signin_link(
            invitation.token.expose(),
            "sextant-and-chart",
            "198.51.100.7",
        )
        .await
        .unwrap();
}

/// A signed-in browser left unattended is still a guessing oracle on the
/// current password, and still 19 MiB of hashing per try.
#[tokio::test]
async fn changing_a_password_is_rate_limited_per_client() {
    let (_scratch, accounts, admin) = claimed().await;
    for _ in 0..RATE_LIMIT {
        let _ = accounts
            .change_password(&admin.id, "not-it", "chronometer-1761", "203.0.113.9")
            .await;
    }
    assert!(matches!(
        accounts
            .change_password(
                &admin.id,
                "tide-tables-1892",
                "chronometer-1761",
                "203.0.113.9"
            )
            .await,
        Err(AccountError::RateLimited)
    ));
}

#[tokio::test]
async fn a_successful_sign_in_clears_the_bucket() {
    let (_scratch, accounts, _admin) = claimed().await;
    for _ in 0..(RATE_LIMIT - 1) {
        let _ = accounts
            .sign_in("ada@izlek.sh", "wrong", "198.51.100.7")
            .await;
    }
    accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
    // Someone who mistypes and then gets it right is not left at the edge.
    assert!(matches!(
        accounts
            .sign_in("ada@izlek.sh", "wrong", "198.51.100.7")
            .await,
        Err(AccountError::Rejected)
    ));
}

#[tokio::test]
async fn changing_a_password_signs_out_every_device() {
    let (_scratch, accounts, admin) = claimed().await;
    let first = accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
    let second = accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.8")
        .await
        .unwrap();
    assert!(
        accounts
            .authenticate(first.session_token.expose())
            .await
            .unwrap()
            .is_some()
    );

    // The baseline is read from the store, not from `second`: sign_in hands
    // back the user it fetched before marking, one arrival stale.
    let seen = accounts
        .store()
        .user(&admin.id)
        .await
        .unwrap()
        .unwrap()
        .last_signed_in_at
        .unwrap();
    let fresh = accounts
        .change_password(
            &admin.id,
            "tide-tables-1892",
            "chronometer-1761",
            "198.51.100.7",
        )
        .await
        .unwrap();
    assert!(
        fresh.user.last_signed_in_at.unwrap() > seen,
        "changing the password is presence"
    );

    for old in [&first, &second] {
        assert!(
            accounts
                .authenticate(old.session_token.expose())
                .await
                .unwrap()
                .is_none(),
            "the pane promises this"
        );
    }
    assert!(
        accounts
            .authenticate(fresh.session_token.expose())
            .await
            .unwrap()
            .is_some()
    );
    accounts
        .sign_in("ada@izlek.sh", "chronometer-1761", "198.51.100.7")
        .await
        .unwrap();
}

#[tokio::test]
async fn changing_a_password_needs_the_current_one_and_obeys_the_rules() {
    let (_scratch, accounts, admin) = claimed().await;
    assert!(matches!(
        accounts
            .change_password(&admin.id, "not-it", "chronometer-1761", "198.51.100.7")
            .await,
        Err(AccountError::Rejected)
    ));
    assert!(matches!(
        accounts
            .change_password(&admin.id, "tide-tables-1892", "short", "198.51.100.7")
            .await,
        Err(AccountError::Password(PasswordProblem::TooShort))
    ));
    // The old password still works after both refusals.
    accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
}

#[tokio::test]
async fn the_old_password_stops_working_after_a_change() {
    let (_scratch, accounts, admin) = claimed().await;
    accounts
        .change_password(
            &admin.id,
            "tide-tables-1892",
            "chronometer-1761",
            "198.51.100.7",
        )
        .await
        .unwrap();
    assert!(matches!(
        accounts
            .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.7")
            .await,
        Err(AccountError::Rejected)
    ));
    accounts
        .sign_in("ada@izlek.sh", "chronometer-1761", "198.51.100.7")
        .await
        .unwrap();
}

#[tokio::test]
async fn signing_out_ends_that_browser_only() {
    let (_scratch, accounts, _admin) = claimed().await;
    let laptop = accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
    let phone = accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.8")
        .await
        .unwrap();

    accounts
        .sign_out(laptop.session_token.expose())
        .await
        .unwrap();
    assert!(
        accounts
            .authenticate(laptop.session_token.expose())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        accounts
            .authenticate(phone.session_token.expose())
            .await
            .unwrap()
            .is_some()
    );
    // Signing out an unknown token is not an error.
    accounts.sign_out(&"0".repeat(32)).await.unwrap();
}

#[tokio::test]
async fn an_address_can_only_be_invited_once() {
    let (_scratch, accounts, admin) = claimed().await;
    accounts
        .invite(&admin, "grace@izlek.sh", "Grace", Role::Member)
        .await
        .unwrap();
    assert!(matches!(
        accounts
            .invite(&admin, "GRACE@izlek.sh", "Grace again", Role::Member)
            .await,
        Err(AccountError::AddressTaken)
    ));
    assert!(matches!(
        accounts
            .invite(&admin, "ada@izlek.sh", "Ada again", Role::Member)
            .await,
        Err(AccountError::AddressTaken)
    ));
}

// -- board ------------------------------------------------------------------

use izlek_core::board::{BoardReads, BoardView, Moved, Person, load};
use izlek_core::store::NewTask;
use std::sync::atomic::{AtomicUsize, Ordering};
use time::Date;
use time::macros::date;

/// Wraps the real store and counts the round trips a board costs.
///
/// This is the whole point of [`BoardReads`] being its own trait: the guard
/// against N+1 is tested by its effect — the count does not move when the board
/// gets bigger — rather than by reading the queries and taking their word.
struct CountingReads<'a> {
    inner: &'a TursoStore,
    calls: AtomicUsize,
}

impl<'a> CountingReads<'a> {
    fn new(inner: &'a TursoStore) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
        }
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn tick(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl BoardReads for CountingReads<'_> {
    async fn board(
        &self,
        workspace_id: &str,
    ) -> Result<Option<izlek_core::board::BoardMeta>, StoreError> {
        self.tick();
        self.inner.board(workspace_id).await
    }

    async fn columns(&self, board_id: &str) -> Result<Vec<izlek_core::board::Column>, StoreError> {
        self.tick();
        self.inner.columns(board_id).await
    }

    async fn tasks_for_board(
        &self,
        board_id: &str,
    ) -> Result<Vec<izlek_core::board::TaskRow>, StoreError> {
        self.tick();
        self.inner.tasks_for_board(board_id).await
    }

    async fn assignees_for_board(
        &self,
        board_id: &str,
    ) -> Result<Vec<(String, Person)>, StoreError> {
        self.tick();
        self.inner.assignees_for_board(board_id).await
    }

    async fn comment_counts_for_board(
        &self,
        board_id: &str,
    ) -> Result<Vec<(String, u32)>, StoreError> {
        self.tick();
        self.inner.comment_counts_for_board(board_id).await
    }

    async fn dependencies_for_board(
        &self,
        board_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        self.tick();
        self.inner.dependencies_for_board(board_id).await
    }
}

async fn board_of(store: &TursoStore, workspace_id: &str) -> BoardView {
    load(store, workspace_id)
        .await
        .unwrap()
        .expect("a claimed workspace has a board")
}

async fn column_named(store: &TursoStore, workspace_id: &str, name: &str) -> String {
    let board = board_of(store, workspace_id).await;
    board
        .columns
        .iter()
        .find(|column| column.column.name == name)
        .unwrap_or_else(|| panic!("no column named {name}"))
        .column
        .id
        .clone()
}

async fn add_task(
    store: &TursoStore,
    workspace_id: &str,
    column: &str,
    title: &str,
    deadline: Option<Date>,
    author: &str,
) -> String {
    let board = store.board(workspace_id).await.unwrap().unwrap();
    let column_id = column_named(store, workspace_id, column).await;
    store
        .create_task(NewTask {
            clock_at: None,
            board_id: &board.id,
            column_id: &column_id,
            parent_id: None,
            title,
            description: "",
            deadline,
            created_by: author,
        })
        .await
        .unwrap()
        .row
        .id
}

/// The counting fns agree with a full read, and a filtered keyset walk
/// covers exactly the filtered set — no row skipped or repeated.
#[tokio::test]
async fn count_and_filtered_keyset_walk_cover_exactly_the_filtered_set() {
    use izlek_core::store::{ActivityFilter, Dir, FeedCursor, FeedPage};

    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let other = member(store, &workspace, "sam@izlek.sh", "Sam").await;
    let t0 = OffsetDateTime::now_utc();
    for i in 0..20 {
        let actor = if i % 2 == 0 { &admin } else { &other };
        store
            .record_event(
                Some(actor),
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("row {i}"),
                t0 + Duration::seconds(i),
            )
            .await
            .unwrap();
    }

    let filter = ActivityFilter {
        actor: Some(admin.clone()),
        ..Default::default()
    };
    let total = store.count_activity(&filter).await.unwrap();
    assert_eq!(total, 10);

    let mut walked = Vec::new();
    let mut page = FeedPage::Newest;
    loop {
        let rows = store
            .recent_activity(3, page, Dir::Newest, &filter)
            .await
            .unwrap();
        if rows.is_empty() {
            break;
        }
        let last = rows.last().unwrap();
        page = FeedPage::Before(FeedCursor {
            at: last.at,
            id: last.id.clone(),
        });
        walked.extend(rows);
    }
    assert_eq!(walked.len(), 10);
    assert!(
        walked
            .iter()
            .all(|r| r.actor_name.as_deref() == Some("Ada"))
    );

    let preceding = store
        .count_activity_preceding(
            &filter,
            Dir::Newest,
            Some(&FeedCursor {
                at: walked[3].at,
                id: walked[3].id.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(preceding, 3);
}

#[tokio::test]
async fn a_claimed_workspace_starts_with_four_named_columns() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;
    let board = board_of(&scratch.store, &workspace).await;

    let names: Vec<&str> = board
        .columns
        .iter()
        .map(|column| column.column.name.as_str())
        .collect();
    assert_eq!(names, ["Backlog", "In Progress", "Review", "Done"]);
    assert!(board.is_empty(), "a fresh board has no cards");
    // Only the last column finishes a task.
    let done: Vec<bool> = board
        .columns
        .iter()
        .map(|column| column.column.is_done)
        .collect();
    assert_eq!(done, [false, false, false, true]);
}

/// A task key's shape: `<prefix>-<5..=7 uppercase Crockford chars>`. The tail
/// comes off the task's own id now, not a per-board counter, so exact keys
/// are no longer predictable — only the shape and per-board uniqueness are.
fn is_task_key_shaped(key: &str, prefix: &str) -> bool {
    let Some(tail) = key
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    (5..=7).contains(&tail.len())
        && tail
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

#[tokio::test]
async fn tasks_get_key_tails_off_their_own_id_not_a_board_counter() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    for title in ["Pricing page draft", "Choose analytics stack"] {
        add_task(store, &workspace, "Backlog", title, None, &admin).await;
    }

    let board = board_of(store, &workspace).await;
    let keys: Vec<&str> = board.cards().map(|card| card.task_key.as_str()).collect();
    assert_eq!(keys.len(), 2);
    for key in &keys {
        assert!(
            is_task_key_shaped(key, "DZ"),
            "key {key} is not shaped like DZ-<5..7 chars>"
        );
    }
    assert_ne!(keys[0], keys[1], "two tasks never share a key");
}

#[tokio::test]
async fn a_card_carries_its_assignees_comments_and_dependency_keys() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let mel = store
        .create_user(NewUser {
            workspace_id: workspace.clone(),
            email: "mel@izlek.sh".into(),
            display_name: "Mel Duarte".into(),
            role: Role::Member,
            invited_by: None,
        })
        .await
        .unwrap();

    let blocking = add_task(
        store,
        &workspace,
        "In Progress",
        "CLI install script (curl | sh)",
        Some(date!(2026 - 08 - 21)),
        &admin,
    )
    .await;
    let blocked = add_task(
        store,
        &workspace,
        "Backlog",
        "Onboarding email sequence",
        Some(date!(2026 - 10 - 06)),
        &admin,
    )
    .await;

    store.assign_task(&blocking, &admin).await.unwrap();
    store.assign_task(&blocking, &mel.id).await.unwrap();
    // Assigning twice is the same as assigning once.
    store.assign_task(&blocking, &mel.id).await.unwrap();
    store
        .add_dependency(&blocked, &blocking, OffsetDateTime::now_utc())
        .await
        .unwrap();
    for body in ["needs a checksum", "and a version pin", "ready for review"] {
        store
            .add_comment(&blocking, &admin, body, OffsetDateTime::now_utc())
            .await
            .unwrap();
    }

    let board = board_of(store, &workspace).await;
    let blocked_key = board
        .cards()
        .find(|card| card.id == blocked)
        .unwrap()
        .task_key
        .clone();
    let card = board.cards().find(|card| card.id == blocking).unwrap();
    assert_eq!(card.assignees.len(), 2);
    assert_eq!(card.comment_count, 3);
    assert_eq!(card.blocks, [blocked_key.as_str()]);
    assert!(card.blocked_by.is_empty());
    assert!(!card.is_blocked());

    let waiting = board.cards().find(|card| card.id == blocked).unwrap();
    assert_eq!(waiting.blocked_by, [card.task_key.as_str()]);
    assert!(waiting.is_blocked());
    assert_eq!(waiting.comment_count, 0);
    assert!(waiting.assignees.is_empty());

    let today = date!(2026 - 08 - 26);
    assert_eq!(card.deadline_label(today), "Aug 21 · overdue");
    assert_eq!(waiting.deadline_label(today), "Oct 06");
    assert_eq!(board.overdue_count(today), 1);
    assert_eq!(board.blocked_count(), 1);
}

#[tokio::test]
async fn a_cleared_dependency_stops_showing_on_the_card() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let blocking = add_task(
        store,
        &workspace,
        "In Progress",
        "Invite flow",
        None,
        &admin,
    )
    .await;
    let blocked = add_task(
        store,
        &workspace,
        "Backlog",
        "Terms of service",
        None,
        &admin,
    )
    .await;
    let now = OffsetDateTime::now_utc();
    store
        .add_dependency(&blocked, &blocking, now)
        .await
        .unwrap();

    let board = board_of(store, &workspace).await;
    assert!(
        board
            .cards()
            .find(|card| card.id == blocked)
            .unwrap()
            .is_blocked()
    );

    store
        .clear_dependency(&blocked, &blocking, now)
        .await
        .unwrap();
    let board = board_of(store, &workspace).await;
    let card = board.cards().find(|card| card.id == blocked).unwrap();
    assert!(card.blocked_by.is_empty());
    assert!(!card.is_blocked());
}

#[tokio::test]
async fn cards_sort_by_deadline_with_the_undated_last() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    for (title, deadline) in [
        ("Choose analytics stack", None),
        ("Terms of service review", Some(date!(2026 - 09 - 30))),
        ("Pricing page draft", Some(date!(2026 - 09 - 12))),
    ] {
        add_task(store, &workspace, "Backlog", title, deadline, &admin).await;
    }

    let board = board_of(store, &workspace).await;
    let titles: Vec<&str> = board.columns[0]
        .cards
        .iter()
        .map(|card| card.title.as_str())
        .collect();
    assert_eq!(
        titles,
        [
            "Pricing page draft",
            "Terms of service review",
            "Choose analytics stack"
        ]
    );
}

#[tokio::test]
async fn a_board_costs_six_queries_whatever_its_size() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;

    let small = CountingReads::new(store);
    load(&small, &workspace).await.unwrap();
    assert_eq!(small.count(), 6, "an empty board");

    // Forty tasks, each with an assignee, a comment and a dependency — the
    // three things a naive card query fans out on.
    let mut previous: Option<String> = None;
    for n in 0..40 {
        let id = add_task(
            store,
            &workspace,
            "Backlog",
            &format!("task {n}"),
            Some(date!(2026 - 09 - 12)),
            &admin,
        )
        .await;
        store.assign_task(&id, &admin).await.unwrap();
        store
            .add_comment(&id, &admin, "a note", OffsetDateTime::now_utc())
            .await
            .unwrap();
        if let Some(previous) = previous.replace(id.clone()) {
            store
                .add_dependency(&id, &previous, OffsetDateTime::now_utc())
                .await
                .unwrap();
        }
    }
    // A tag rides the task query as a join: the sweep stays at six.
    let board_meta = store.board(&workspace).await.unwrap().unwrap();
    let tag = store
        .create_tag(&board_meta.id, "shipping", OffsetDateTime::now_utc())
        .await
        .unwrap();
    let tagged = previous.expect("the loop created tasks");
    store.set_task_tag(&tagged, &tag.id).await.unwrap();

    let big = CountingReads::new(store);
    let board = load(&big, &workspace).await.unwrap().unwrap();
    assert_eq!(board.task_count(), 40);
    let tagged_card = board
        .columns
        .iter()
        .flat_map(|column| column.cards.iter())
        .find(|card| card.id == tagged)
        .unwrap();
    assert_eq!(
        tagged_card.tag.as_ref().map(|t| t.name.as_str()),
        Some("shipping")
    );
    assert_eq!(
        big.count(),
        6,
        "the round trips a board costs must not follow the number of tasks"
    );
}

// -- tags -------------------------------------------------------------------

#[tokio::test]
async fn tags_are_created_in_order_renamed_moved_and_deleted() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let at = OffsetDateTime::now_utc();

    let alpha = store.create_tag(&board.id, "alpha", at).await.unwrap();
    let beta = store.create_tag(&board.id, "beta", at).await.unwrap();
    let gamma = store.create_tag(&board.id, "gamma", at).await.unwrap();
    assert_eq!((alpha.position, beta.position, gamma.position), (1, 2, 3));

    store.rename_tag(&beta.id, "beta2").await.unwrap();
    assert_eq!(
        tag_order(&store.tags(&board.id).await.unwrap()),
        ["General", "alpha", "beta2", "gamma"]
    );

    // Swapping moves one place, never to the far end.
    store.move_tag(&beta.id, true).await.unwrap();
    assert_eq!(
        tag_order(&store.tags(&board.id).await.unwrap()),
        ["General", "beta2", "alpha", "gamma"]
    );
    // A tag already at that end stays put: nothing to swap is not an error.
    store.move_tag(&gamma.id, false).await.unwrap();
    assert_eq!(
        tag_order(&store.tags(&board.id).await.unwrap()),
        ["General", "beta2", "alpha", "gamma"]
    );
    store.move_tag(&beta.id, true).await.unwrap();
    assert_eq!(
        tag_order(&store.tags(&board.id).await.unwrap()),
        ["beta2", "General", "alpha", "gamma"]
    );
    store.move_tag(&alpha.id, true).await.unwrap();
    assert_eq!(
        tag_order(&store.tags(&board.id).await.unwrap()),
        ["beta2", "alpha", "General", "gamma"]
    );

    store.delete_tag(&beta.id).await.unwrap();
    assert_eq!(
        tag_order(&store.tags(&board.id).await.unwrap()),
        ["alpha", "General", "gamma"]
    );
    assert!(matches!(
        store.delete_tag(&beta.id).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_boards_tags_have_unique_names() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let at = OffsetDateTime::now_utc();

    store.create_tag(&board.id, "ops", at).await.unwrap();
    assert!(
        matches!(
            store.create_tag(&board.id, "ops", at).await,
            Err(StoreError::Conflict("tag"))
        ),
        "two tags with one name are one project spelled twice"
    );

    // The same name on another board is a different project.
    let other = Scratch::open().await;
    let (other_ws, _other_admin) = claim(&other.store).await;
    let other_board = other.store.board(&other_ws).await.unwrap().unwrap();
    other
        .store
        .create_tag(&other_board.id, "ops", at)
        .await
        .unwrap();

    // A rename into a clash refuses the same way.
    let ship = store.create_tag(&board.id, "ship", at).await.unwrap();
    assert!(matches!(
        store.rename_tag(&ship.id, "ops").await,
        Err(StoreError::Conflict("tag"))
    ));
}

fn tag_order(tags: &[izlek_core::store::Tag]) -> Vec<&str> {
    tags.iter().map(|t| t.name.as_str()).collect()
}

fn card_of<'a>(
    view: &'a izlek_core::board::BoardView,
    task: &str,
) -> &'a izlek_core::board::TaskCard {
    view.columns
        .iter()
        .flat_map(|column| column.cards.iter())
        .find(|card| card.id == task)
        .unwrap()
}

#[tokio::test]
async fn a_tag_is_worn_a_foreign_one_is_not_found_and_a_worn_one_is_not_deletable() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let tag = store
        .create_tag(&board.id, "ops", OffsetDateTime::now_utc())
        .await
        .unwrap();
    let task = add_task(store, &workspace, "Backlog", "tagged", None, &admin).await;

    // The task came wearing the default; moving it to `ops` shows in the
    // card and the detail alike.
    let view = board_of(store, &workspace).await;
    assert_eq!(
        card_of(&view, &task).tag.as_ref().map(|t| t.name.as_str()),
        Some("General")
    );
    store.set_task_tag(&task, &tag.id).await.unwrap();
    let view = board_of(store, &workspace).await;
    assert_eq!(
        card_of(&view, &task).tag.as_ref().map(|t| t.name.as_str()),
        Some("ops")
    );
    let detail = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        detail.tag.as_ref().map(|t| t.id.as_str()),
        Some(tag.id.as_str())
    );

    // A tag from another board — or one that never existed — is not found,
    // not refused: it is not one of this board's projects at all.
    let other = Scratch::open().await;
    let (other_ws, _other_admin) = claim(&other.store).await;
    let other_board = other.store.board(&other_ws).await.unwrap().unwrap();
    let foreign = other
        .store
        .create_tag(&other_board.id, "foreign", OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(matches!(
        store.set_task_tag(&task, &foreign.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.set_task_tag(&task, "no-such-tag").await,
        Err(StoreError::NotFound)
    ));

    // A tag with a card on it is not deletable: the card is the reason the
    // project exists, and nothing about it moves.
    assert!(matches!(
        store.delete_tag(&tag.id).await,
        Err(StoreError::Conflict("tag_in_use"))
    ));
    let view = board_of(store, &workspace).await;
    assert_eq!(
        card_of(&view, &task).tag.as_ref().map(|t| t.name.as_str()),
        Some("ops"),
        "the refused delete moved the card anyway"
    );
    assert_eq!(store.tags(&board.id).await.unwrap().len(), 2);

    // Emptied, it goes.
    let default = store
        .tags(&board.id)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.is_default)
        .unwrap();
    store.set_task_tag(&task, &default.id).await.unwrap();
    store.delete_tag(&tag.id).await.unwrap();
    assert_eq!(store.tags(&board.id).await.unwrap().len(), 1);
}

/// The counts the tags screen draws, and the one card that stops counting.
#[tokio::test]
async fn tag_counts_hold_the_live_cards_and_a_thrown_away_card_stops_blocking() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let tag = store
        .create_tag(&board.id, "ops", OffsetDateTime::now_utc())
        .await
        .unwrap();
    let first = add_task(store, &workspace, "Backlog", "one", None, &admin).await;
    let second = add_task(store, &workspace, "Backlog", "two", None, &admin).await;
    store.set_task_tag(&first, &tag.id).await.unwrap();
    store.set_task_tag(&second, &tag.id).await.unwrap();

    let counts = store.tag_task_counts(&board.id).await.unwrap();
    assert_eq!(
        counts.iter().find(|(id, _)| id == &tag.id).map(|(_, n)| *n),
        Some(2)
    );

    // A card thrown away is nobody's work any more: it stops counting, and
    // once the last live one is gone the tag can be retired — the deleted
    // card's own reference moves to the default rather than dangling.
    store
        .delete_task(&first, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(matches!(
        store.delete_tag(&tag.id).await,
        Err(StoreError::Conflict("tag_in_use")),
    ));
    store
        .delete_task(&second, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(
        store
            .tag_task_counts(&board.id)
            .await
            .unwrap()
            .iter()
            .find(|(id, _)| id == &tag.id),
        None,
        "a tag nobody wears is absent, not zero"
    );
    store.delete_tag(&tag.id).await.unwrap();

    // Nothing points at a tag that is not there.
    let mut broken = raw_conn(&scratch)
        .await
        .query("PRAGMA foreign_key_check", ())
        .await
        .unwrap();
    assert!(broken.next().await.unwrap().is_none());
}

#[tokio::test]
async fn a_fresh_board_comes_with_a_default_tag_and_new_tasks_wear_it() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let board = store.board(&workspace).await.unwrap().unwrap();

    let tags = store.tags(&board.id).await.unwrap();
    assert_eq!(tags.len(), 1, "a claimed board seeds exactly one tag");
    assert!(tags[0].is_default, "the seeded tag is the default");
    assert_eq!((tags[0].name.as_str(), tags[0].position), ("General", 0));

    // Nobody chose a tag; the task wears the board's default all the same.
    let task = add_task(store, &workspace, "Backlog", "first", None, &admin).await;
    let view = board_of(store, &workspace).await;
    assert_eq!(
        card_of(&view, &task).tag.as_ref().map(|t| t.name.as_str()),
        Some("General")
    );
}

#[tokio::test]
async fn the_default_tag_cannot_be_deleted_but_renames_and_moves_like_any_other() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let default = &store.tags(&board.id).await.unwrap()[0];

    assert!(matches!(
        store.delete_tag(&default.id).await,
        Err(StoreError::Conflict("default_tag"))
    ));

    store.rename_tag(&default.id, "Projects").await.unwrap();
    store
        .create_tag(&board.id, "alpha", OffsetDateTime::now_utc())
        .await
        .unwrap();
    store.move_tag(&default.id, false).await.unwrap();
    assert_eq!(
        tag_order(&store.tags(&board.id).await.unwrap()),
        ["alpha", "Projects"]
    );
}

#[tokio::test]
async fn a_board_cannot_hold_two_defaults_and_a_task_cannot_go_tagless() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;
    let board = scratch.store.board(&workspace).await.unwrap().unwrap();

    // Two defaults is a schema rule, so it is the schema that refuses —
    // exercised raw, because the store's own writes never try it.
    let conn = raw_conn(&scratch).await;
    let _ = conn
        .execute(
            "INSERT INTO tag (id, board_id, name, position, is_default, created_at) \
             VALUES ('t2', ?1, 'second', 9, 1, '2026-01-01T00:00:00Z')",
            turso::params![board.id.clone()],
        )
        .await
        .expect_err("a second default tag violates tag_one_default");

    // And a task's tag_id is NOT NULL in the declared schema itself, not a
    // habit of the write path.
    let _ = conn
        .execute(
            "INSERT INTO task (id, board_id, task_key, title, column_id, tag_id, position, \
             created_by, created_at, updated_at) \
             VALUES ('tk', ?1, 'DZ-99', 'raw', (SELECT id FROM board_column LIMIT 1), NULL, 0, \
             (SELECT id FROM user LIMIT 1), '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            turso::params![board.id.clone()],
        )
        .await
        .expect_err("task.tag_id is NOT NULL");
}

#[test]
fn initials_fall_back_to_two_letters() {
    let person = |name: &str| Person {
        id: "u".into(),
        display_name: name.into(),
        has_photo: false,
    };
    assert_eq!(person("Mel Duarte").initials(), "MD");
    assert_eq!(person("Ada").initials(), "A");
    assert_eq!(person("  ").initials(), "?");
}

// -- task detail ------------------------------------------------------------

use izlek_core::detail::{
    ActivityEntry, ActivityKind, Comment, DependencyEdge, DetailReads, TaskFacts,
    load as load_detail,
};

/// The same trick as [`CountingReads`], for the detail screen: the round trips
/// one task costs must not follow how much it carries.
struct CountingDetail<'a> {
    inner: &'a TursoStore,
    calls: AtomicUsize,
}

impl<'a> CountingDetail<'a> {
    fn new(inner: &'a TursoStore) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
        }
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn tick(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl DetailReads for CountingDetail<'_> {
    async fn task(&self, task_id: &str) -> Result<Option<TaskFacts>, StoreError> {
        self.tick();
        self.inner.task(task_id).await
    }

    async fn columns_for_board(
        &self,
        board_id: &str,
    ) -> Result<Vec<izlek_core::Column>, StoreError> {
        self.tick();
        self.inner.columns_for_board(board_id).await
    }

    async fn assignees_for_task(&self, task_id: &str) -> Result<Vec<Person>, StoreError> {
        self.tick();
        self.inner.assignees_for_task(task_id).await
    }

    async fn assignable_people(&self, workspace_id: &str) -> Result<Vec<Person>, StoreError> {
        self.tick();
        self.inner.assignable_people(workspace_id).await
    }

    async fn dependencies_for_task(
        &self,
        task_id: &str,
    ) -> Result<Vec<(bool, DependencyEdge)>, StoreError> {
        self.tick();
        self.inner.dependencies_for_task(task_id).await
    }

    async fn comments_for_task(&self, task_id: &str) -> Result<Vec<Comment>, StoreError> {
        self.tick();
        self.inner.comments_for_task(task_id).await
    }

    async fn files_for_task(
        &self,
        task_id: &str,
    ) -> Result<Vec<izlek_core::detail::FileLine>, StoreError> {
        self.tick();
        self.inner.files_for_task(task_id).await
    }

    async fn activity_for_task(&self, task_id: &str) -> Result<Vec<ActivityEntry>, StoreError> {
        self.tick();
        self.inner.activity_for_task(task_id).await
    }

    async fn family_for_task(
        &self,
        task_id: &str,
    ) -> Result<Vec<(bool, izlek_core::detail::SubtaskLine)>, StoreError> {
        self.tick();
        self.inner.family_for_task(task_id).await
    }
}

#[tokio::test]
async fn a_task_detail_carries_both_directions_of_its_dependencies() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let middle = add_task(store, &workspace, "Backlog", "middle", None, &admin).await;
    let before = add_task(store, &workspace, "Backlog", "before", None, &admin).await;
    let after = add_task(store, &workspace, "Backlog", "after", None, &admin).await;
    store.add_dependency(&middle, &before, now).await.unwrap();
    store.add_dependency(&after, &middle, now).await.unwrap();

    let detail = load_detail(store, &workspace, &middle)
        .await
        .unwrap()
        .expect("the task is in this workspace");
    assert_eq!(detail.title, "middle");
    assert_eq!(detail.column.name, "Backlog");
    assert_eq!(detail.columns.len(), 4, "every column, for the picker");
    let blocked_by: Vec<&str> = detail
        .blocked_by
        .iter()
        .map(|edge| edge.title.as_str())
        .collect();
    let blocks: Vec<&str> = detail.blocks.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(blocked_by, ["before"]);
    assert_eq!(blocks, ["after"]);
    assert!(detail.is_blocked());
    assert_eq!(
        detail.blocked_by[0].blocked_by_label(),
        "blocking this task"
    );
    assert_eq!(detail.blocks[0].blocks_label(), "waiting on this task");
}

#[tokio::test]
async fn a_viewer_is_never_offered_as_an_assignee() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let quiet = store
        .create_user(NewUser {
            workspace_id: workspace.clone(),
            email: "quiet@izlek.sh".into(),
            display_name: "Quiet Reader".into(),
            role: Role::Viewer,
            invited_by: None,
        })
        .await
        .unwrap();
    let task = add_task(store, &workspace, "Backlog", "a task", None, &admin).await;

    let detail = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !detail.assignable.iter().any(|p| p.id == quiet.id),
        "a viewer cannot be given work, so the picker never lists one"
    );
    assert!(detail.assignable.iter().any(|p| p.id == admin));
}

#[tokio::test]
async fn a_task_in_another_workspace_is_not_found_rather_than_forbidden() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let task = add_task(store, &workspace, "Backlog", "a task", None, &admin).await;

    // Asking as a workspace that does not hold it says nothing about whether
    // the id is real.
    let answer = load_detail(store, "some-other-workspace", &task)
        .await
        .unwrap();
    assert!(answer.is_none());
}

#[tokio::test]
async fn a_dependency_that_would_close_a_circle_is_refused() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let a = add_task(store, &workspace, "Backlog", "a", None, &admin).await;
    let b = add_task(store, &workspace, "Backlog", "b", None, &admin).await;
    let c = add_task(store, &workspace, "Backlog", "c", None, &admin).await;

    // a waits on b, b waits on c. Now ask for c to wait on a: three nodes, not
    // a self-edge, and the loop only shows up on the second hop.
    store.add_dependency(&a, &b, now).await.unwrap();
    store.add_dependency(&b, &c, now).await.unwrap();
    assert!(matches!(
        store.add_dependency(&c, &a, now).await,
        Err(StoreError::Cycle)
    ));
    assert!(matches!(
        store.add_dependency(&a, &a, now).await,
        Err(StoreError::Cycle)
    ));

    let detail = load_detail(store, &workspace, &c).await.unwrap().unwrap();
    assert!(detail.blocked_by.is_empty());
}

#[tokio::test]
async fn a_cleared_dependency_is_not_a_circle() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let a = add_task(store, &workspace, "Backlog", "a", None, &admin).await;
    let b = add_task(store, &workspace, "Backlog", "b", None, &admin).await;
    store.add_dependency(&a, &b, now).await.unwrap();
    store.clear_dependency(&a, &b, now).await.unwrap();

    // `cleared_at` is set by an unlink and nothing else, and a cleared row no
    // longer appears on the screen. Refusing the other direction would refuse a
    // circle nobody can see — and leave the pair unlinkable for good, because
    // the cleared row never goes away. A link whose blocker is merely finished
    // is a different thing: that is `done_at`, the edge is live, and it still
    // walks.
    store.add_dependency(&b, &a, now).await.unwrap();

    let detail = load_detail(store, &workspace, &b).await.unwrap().unwrap();
    assert_eq!(detail.blocked_by.len(), 1, "the new link stands");
    let detail = load_detail(store, &workspace, &a).await.unwrap().unwrap();
    assert!(detail.blocked_by.is_empty(), "the cleared one is gone");
}

#[tokio::test]
async fn a_finished_blocker_still_counts_as_a_circle() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let a = add_task(store, &workspace, "Backlog", "a", None, &admin).await;
    let b = add_task(store, &workspace, "Backlog", "b", None, &admin).await;
    store.add_dependency(&a, &b, now).await.unwrap();
    let done = column_named(store, &workspace, "Done").await;
    let backlog = column_named(store, &workspace, "Backlog").await;
    store
        .move_task(&b, &backlog, &done, &admin, now)
        .await
        .unwrap();

    // b is finished, so the row reads as cleared on the screen — but the link
    // is still in force, and the other direction is still a circle.
    assert!(matches!(
        store.add_dependency(&b, &a, now).await,
        Err(StoreError::Cycle)
    ));
}

#[tokio::test]
async fn deleting_a_task_frees_what_was_waiting_on_it() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let blocking = add_task(store, &workspace, "Backlog", "blocking", None, &admin).await;
    let other = add_task(store, &workspace, "Backlog", "other", None, &admin).await;
    let freed = add_task(store, &workspace, "Backlog", "freed", None, &admin).await;
    let still_stuck = add_task(store, &workspace, "Backlog", "stuck", None, &admin).await;
    store.add_dependency(&freed, &blocking, now).await.unwrap();
    store
        .add_dependency(&still_stuck, &blocking, now)
        .await
        .unwrap();
    store
        .add_dependency(&still_stuck, &other, now)
        .await
        .unwrap();

    let deletion = store.delete_task(&blocking, &admin, now).await.unwrap();
    assert_eq!(
        deletion.freed,
        vec![freed.clone()],
        "only the one with nothing else in front of it"
    );
    assert!(
        deletion.event.is_some(),
        "a delete that freed somebody is an event the rules can fire on"
    );
    match store.event(&deletion.activity_id).await.unwrap() {
        Some(Event::Happened(ev)) => assert_eq!(ev.kind, ActivityKind::Deleted),
        other => panic!("expected a Happened Deleted event, got {other:?}"),
    }

    // The deleted task is gone from the board and from the edges it stood in.
    let board = board_of(store, &workspace).await;
    assert!(
        !board
            .columns
            .iter()
            .flat_map(|column| column.cards.iter())
            .any(|card| card.id == blocking)
    );
    let freed_detail = load_detail(store, &workspace, &freed)
        .await
        .unwrap()
        .unwrap();
    assert!(freed_detail.blocked_by.is_empty());
    assert!(!freed_detail.is_blocked());

    // And the freeing is a recorded event, because the rules engine will want
    // to mail about it.
    let unblocked_line = freed_detail
        .activity
        .iter()
        .find(|entry| entry.kind == ActivityKind::Unblocked)
        .expect("the freeing is on the record");
    assert!(
        unblocked_line.actor.is_none(),
        "the system did this, not a person"
    );
    assert!(unblocked_line.sentence().starts_with("unblocked this task"));

    let stuck_detail = load_detail(store, &workspace, &still_stuck)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stuck_detail.is_blocked(),
        "something else is still in front of it"
    );
    assert!(
        !stuck_detail
            .activity
            .iter()
            .any(|entry| entry.kind == ActivityKind::Unblocked)
    );

    // A second delete finds nothing to delete.
    assert!(matches!(
        store.delete_task(&blocking, &admin, now).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn saving_a_task_records_only_what_changed() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let task = add_task(store, &workspace, "Backlog", "first title", None, &admin).await;

    let ids = store
        .save_task(
            &task,
            "first title",
            "",
            None,
            None,
            &admin,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert_eq!(
        ids,
        Vec::<String>::new(),
        "a save that changed nothing writes nothing"
    );
    let detail = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();
    let kinds: Vec<&ActivityKind> = detail.activity.iter().map(|e| &e.kind).collect();
    assert_eq!(
        kinds,
        [&ActivityKind::Created],
        "a save that changed nothing says nothing"
    );

    let ids = store
        .save_task(
            &task,
            "second title",
            "some prose",
            Some(date!(2026 - 09 - 12)),
            None,
            &admin,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let expected_kinds = [
        ActivityKind::Retitled,
        ActivityKind::Described,
        ActivityKind::DeadlineSet,
    ];
    assert_eq!(ids.len(), expected_kinds.len());
    for (id, kind) in ids.iter().zip(expected_kinds) {
        match store.event(id).await.unwrap() {
            Some(Event::Happened(ev)) => assert_eq!(ev.kind, kind),
            other => panic!("expected a Happened event for {id}, got {other:?}"),
        }
    }
    let detail = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.title, "second title");
    assert_eq!(detail.description, "some prose");
    assert_eq!(detail.deadline_input(), "2026-09-12");
    // Newest first, as everywhere else: the deadline was the last thing set.
    let kinds: Vec<&ActivityKind> = detail.activity.iter().map(|e| &e.kind).collect();
    assert_eq!(
        kinds,
        [
            &ActivityKind::DeadlineSet,
            &ActivityKind::Described,
            &ActivityKind::Retitled,
            &ActivityKind::Created
        ]
    );
}

#[tokio::test]
async fn a_task_detail_costs_nine_queries_whatever_it_carries() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let bare = add_task(store, &workspace, "Backlog", "bare", None, &admin).await;
    let counted = CountingDetail::new(store);
    load_detail(&counted, &workspace, &bare).await.unwrap();
    assert_eq!(counted.count(), 9, "a task with nothing hung off it");

    // Twenty comments, twenty activity lines, twenty people who could be
    // assigned, twenty tasks on the other end of a dependency and twenty
    // subtasks each with an assignee — the things a naive detail query fans
    // out on.
    let heavy = add_task(store, &workspace, "Backlog", "heavy", None, &admin).await;
    for n in 0..20 {
        let person = store
            .create_user(NewUser {
                workspace_id: workspace.clone(),
                email: format!("member{n}@izlek.sh"),
                display_name: format!("Member {n}"),
                role: Role::Member,
                invited_by: None,
            })
            .await
            .unwrap();
        store.assign_task(&heavy, &person.id).await.unwrap();
        store
            .add_comment(&heavy, &person.id, "a note", now)
            .await
            .unwrap();
        store
            .record_activity(
                &heavy,
                Some(&person.id),
                None,
                &ActivityKind::Moved,
                "to Review",
                now,
            )
            .await
            .unwrap();
        let neighbour =
            add_task(store, &workspace, "Backlog", &format!("n{n}"), None, &admin).await;
        store.add_dependency(&neighbour, &heavy, now).await.unwrap();

        let part = add_task(store, &workspace, "Backlog", &format!("p{n}"), None, &admin).await;
        store.set_parent(&part, Some(&heavy)).await.unwrap();
        store.assign_task(&part, &person.id).await.unwrap();
    }

    let counted = CountingDetail::new(store);
    let detail = load_detail(&counted, &workspace, &heavy)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.comments.len(), 20);
    assert_eq!(detail.assignees.len(), 20);
    assert_eq!(detail.blocks.len(), 20);
    assert_eq!(detail.subtasks.len(), 20);
    assert!(detail.parent.is_none());
    assert!(
        detail.subtasks.iter().all(|part| part.assignees.len() == 1),
        "a subtask row carries who holds it"
    );
    // Twenty comments (each its own Commented line), twenty moves, plus the
    // line create_task wrote.
    assert_eq!(detail.activity.len(), 41);
    assert_eq!(
        counted.count(),
        9,
        "the round trips a detail costs must not follow what it carries"
    );
}

#[tokio::test]
async fn a_delete_says_what_it_would_take_with_it() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let doomed = add_task(store, &workspace, "Backlog", "doomed", None, &admin).await;
    let other = add_task(store, &workspace, "Backlog", "other", None, &admin).await;
    let freed = add_task(store, &workspace, "Backlog", "freed", None, &admin).await;
    let still_stuck = add_task(store, &workspace, "Backlog", "stuck", None, &admin).await;
    store.add_dependency(&freed, &doomed, now).await.unwrap();
    store
        .add_dependency(&still_stuck, &doomed, now)
        .await
        .unwrap();
    store
        .add_dependency(&still_stuck, &other, now)
        .await
        .unwrap();
    store
        .add_comment(&doomed, &admin, "one", now)
        .await
        .unwrap();
    store
        .add_comment(&doomed, &admin, "two", now)
        .await
        .unwrap();

    let cost = store.deletion_cost(&doomed).await.unwrap().unwrap();
    assert_eq!(cost.title, "doomed");
    assert_eq!(cost.comment_count, 2);
    assert_eq!(cost.link_count, 2, "both tasks waiting on this one");

    // The same reading the delete itself takes: only the task with nothing
    // else in front of it is named.
    let freed_key = key_of(store, &workspace, "freed").await;
    assert_eq!(cost.frees, vec![freed_key]);

    // And it is a preview: nothing was written.
    assert_eq!(
        store.delete_task(&doomed, &admin, now).await.unwrap().freed,
        vec![freed.clone()]
    );
    assert!(store.deletion_cost(&doomed).await.unwrap().is_none());
}

/// The key the board shows for a task with this title.
async fn key_of(store: &TursoStore, workspace: &str, title: &str) -> String {
    let board = board_of(store, workspace).await;
    board
        .columns
        .iter()
        .flat_map(|column| column.cards.iter())
        .find(|card| card.title == title)
        .expect("no such card")
        .task_key
        .clone()
}

// -- moving a card ---------------------------------------------------------

#[tokio::test]
async fn a_move_writes_the_crossing_in_the_same_breath() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let backlog = column_named(store, &workspace, "Backlog").await;
    let progress = column_named(store, &workspace, "In Progress").await;
    let task = add_task(
        store,
        &workspace,
        "Backlog",
        "wire the sender",
        None,
        &admin,
    )
    .await;

    let moved = store
        .move_task(&task, &backlog, &progress, &admin, now)
        .await
        .unwrap();

    let Moved::Recorded(transition) = moved else {
        panic!("a card that changed column has a transition, got {moved:?}");
    };
    assert_eq!(transition.task_id, task);
    assert_eq!(transition.from_column, backlog);
    assert_eq!(transition.to_column, progress);
    assert_eq!(transition.actor_id, admin);
    // The mail engine reads this timestamp rather than its own clock, so it is
    // the move's moment and not the send's.
    assert_eq!(transition.at, now);

    let board = board_of(store, &workspace).await;
    let card = board.cards().find(|card| card.id == task).unwrap();
    assert_eq!(card.column_id, progress);

    let detail = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();
    let line = detail
        .activity
        .iter()
        .find(|line| line.kind == ActivityKind::Moved)
        .expect("the move is in the activity trail");
    assert_eq!(
        line.actor.as_ref().map(|who| who.id.as_str()),
        Some(admin.as_str())
    );
    assert_eq!(line.detail, "Backlog to In Progress");
}

#[tokio::test]
async fn a_card_dropped_back_where_it_came_from_did_not_move() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let backlog = column_named(store, &workspace, "Backlog").await;
    let task = add_task(store, &workspace, "Backlog", "stays put", None, &admin).await;
    let before = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();

    let moved = store
        .move_task(&task, &backlog, &backlog, &admin, now)
        .await
        .unwrap();

    assert_eq!(moved, Moved::Unchanged);
    let after = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.activity.len(),
        before.activity.len(),
        "a drop that changed nothing wrote nothing"
    );
}

#[tokio::test]
async fn two_drags_of_one_card_leave_one_crossing() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let backlog = column_named(store, &workspace, "Backlog").await;
    let progress = column_named(store, &workspace, "In Progress").await;
    let review = column_named(store, &workspace, "Review").await;
    let task = add_task(store, &workspace, "Backlog", "contested", None, &admin).await;

    // Both people picked the card up out of Backlog. Only one drop can be
    // acting on the board that was actually there.
    let first = store
        .move_task(&task, &backlog, &progress, &admin, now)
        .await
        .unwrap();
    let second = store
        .move_task(&task, &backlog, &review, &admin, now)
        .await
        .unwrap();

    assert!(matches!(first, Moved::Recorded(_)));
    assert_eq!(
        second,
        Moved::Stale,
        "the second drop is not allowed to cross out of a column the card had left"
    );

    let detail = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();
    let crossings = detail
        .activity
        .iter()
        .filter(|line| line.kind == ActivityKind::Moved)
        .count();
    assert_eq!(crossings, 1);
    let board = board_of(store, &workspace).await;
    let card = board.cards().find(|card| card.id == task).unwrap();
    assert_eq!(card.column_id, progress, "the winner's move stands");
}

#[tokio::test]
async fn the_done_column_stamps_the_card_and_leaving_it_unstamps() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let backlog = column_named(store, &workspace, "Backlog").await;
    let done = column_named(store, &workspace, "Done").await;
    let task = add_task(store, &workspace, "Backlog", "finishes", None, &admin).await;

    store
        .move_task(&task, &backlog, &done, &admin, now)
        .await
        .unwrap();
    let board = board_of(store, &workspace).await;
    let card = board.cards().find(|card| card.id == task).unwrap();
    assert!(card.is_done(), "a card in the done column is done");
    assert!(card.done_at.is_some());

    store
        .move_task(&task, &done, &backlog, &admin, now)
        .await
        .unwrap();
    let board = board_of(store, &workspace).await;
    let card = board.cards().find(|card| card.id == task).unwrap();
    assert!(!card.is_done(), "dragged back out, it is not finished");
    assert!(card.done_at.is_none());
}

#[tokio::test]
async fn a_column_from_another_board_is_not_a_destination() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let backlog = column_named(store, &workspace, "Backlog").await;
    let task = add_task(store, &workspace, "Backlog", "stays here", None, &admin).await;

    let refused = store
        .move_task(&task, &backlog, "not-a-column", &admin, now)
        .await;

    assert!(matches!(refused, Err(StoreError::NotFound)));
    let board = board_of(store, &workspace).await;
    let card = board.cards().find(|card| card.id == task).unwrap();
    assert_eq!(card.column_id, backlog, "nothing moved");
}

#[tokio::test]
async fn a_deleted_card_does_not_move() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let backlog = column_named(store, &workspace, "Backlog").await;
    let progress = column_named(store, &workspace, "In Progress").await;
    let task = add_task(store, &workspace, "Backlog", "gone", None, &admin).await;
    store.delete_task(&task, &admin, now).await.unwrap();

    let refused = store
        .move_task(&task, &backlog, &progress, &admin, now)
        .await;

    assert!(matches!(refused, Err(StoreError::NotFound)));
}

/// The schema's REFERENCES clauses are only worth anything if the engine acts
/// on them. A drive database once held members pointing at a workspace id with
/// no row behind it — those rows went in through the `sqlite3` CLI, which has
/// `foreign_keys` OFF by default, but the only way to know that is to prove the
/// store's own connections have it ON and refuse the same insert.
#[tokio::test]
async fn an_orphan_row_is_refused() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;

    // Through the store's own API first: a user needs a workspace that exists.
    let refused = scratch
        .store
        .create_user(NewUser {
            workspace_id: "no-such-workspace".into(),
            email: "orphan@izlek.sh".into(),
            display_name: "Orphan".into(),
            role: Role::Member,
            invited_by: None,
        })
        .await;
    assert!(
        refused.is_err(),
        "a user in a workspace that does not exist was accepted"
    );

    // And at the engine, where the pragma either fired or did not. Turso is
    // single-writer, so this gets a file of its own rather than a second handle
    // on the live one, which would fail with "database is locked" and tell us
    // nothing about foreign keys.
    let existing = scratch
        .store
        .users(&workspace)
        .await
        .expect("the workspace reads back")
        .len();
    assert_eq!(existing, 1, "only the admin");

    let dir = std::env::temp_dir().join(format!("izlek-fk-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_str().unwrap().to_owned();
    {
        TursoStore::open(&path).await.unwrap();
    }

    let db = turso::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute("PRAGMA foreign_keys = ON", ()).await.unwrap();
    let mut rows = conn.query("PRAGMA foreign_keys", ()).await.unwrap();
    let on = rows.next().await.unwrap().expect("the pragma reads back");
    assert_eq!(
        on.get::<i64>(0).unwrap(),
        1,
        "foreign_keys did not stay ON for this connection"
    );
    drop(rows);

    let refused = conn
        .execute(
            "INSERT INTO user (id, workspace_id, email, display_name, role, created_at) \
             VALUES ('u-orphan', 'no-such-workspace', 'orphan@izlek.sh', 'Orphan', \
             'member', '2026-08-26')",
            (),
        )
        .await;
    assert!(
        refused.is_err(),
        "the engine accepted a user pointing at no workspace"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// -- mail rules and the send ledger ---------------------------------------

async fn moved_to(
    store: &TursoStore,
    workspace: &str,
    task: &str,
    from: &str,
    to: &str,
    actor: &str,
) -> izlek_core::board::Transition {
    let from_id = column_named(store, workspace, from).await;
    let to_id = column_named(store, workspace, to).await;
    match store
        .move_task(task, &from_id, &to_id, actor, OffsetDateTime::now_utc())
        .await
        .unwrap()
    {
        Moved::Recorded(transition) => transition,
        other => panic!("the move did not happen: {other:?}"),
    }
}

async fn a_rule(store: &TursoStore, workspace: &str, column: &str, subject: &str) -> MailRule {
    let board = store.board(workspace).await.unwrap().unwrap();
    let column_id = column_named(store, workspace, column).await;
    store
        .create_mail_rule(
            &board.id,
            &Trigger::StatusBecomes(Some(column_id)),
            subject,
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_rule_naming_a_column_it_cannot_act_on_is_refused_by_the_schema() {
    // The store's own API cannot build a rule like these, so this reaches
    // past it, straight at the table. The check constraint is the guard: a
    // column on a trigger that never reads one is a rule whose author meant
    // something the engine will not do.
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();
    let store = TursoStore::open(&path).await.unwrap();
    let (workspace, _admin) = claim(&store).await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    drop(store);

    let db = turso::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    // Foreign keys are a per-connection pragma, and this connection is not one
    // the store handed out — so it says so itself, the way the store does on
    // every connection it opens.
    conn.execute("PRAGMA foreign_keys = ON", ()).await.unwrap();
    let refused = conn
        .execute(
            "INSERT INTO mail_rule \
             (id, board_id, trigger_kind, trigger_column, subject, audience, enabled, created_at) \
             SELECT 'r1', ?1, 'created', id, 'Task completed', 'assignees', 1, '2026-08-26' \
             FROM board_column LIMIT 1",
            turso::params![board.id.clone()],
        )
        .await;
    assert!(
        refused.is_err(),
        "a rule that reads no column was stored carrying one"
    );

    // A status rule with no column is not half-written: it is the rule that
    // watches every column, and the schema has to let it through.
    let every_column = conn
        .execute(
            "INSERT INTO mail_rule \
             (id, board_id, trigger_kind, trigger_column, subject, audience, enabled, created_at) \
             VALUES ('r3', ?1, 'status', NULL, 'It moved', 'assignees', 1, '2026-08-26')",
            turso::params![board.id.clone()],
        )
        .await;
    assert!(every_column.is_ok(), "the every-column rule was refused");

    let missing_column = conn
        .execute(
            "INSERT INTO mail_rule \
             (id, board_id, trigger_kind, trigger_column, subject, audience, enabled, created_at) \
             VALUES ('r2', ?1, 'status', 'no-such-column', 'Task completed', 'assignees', 1, \
                     '2026-08-26')",
            turso::params![board.id],
        )
        .await;
    assert!(
        missing_column.is_err(),
        "a rule pointed at a column that is not there was stored"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rule_is_one_sentence_and_starts_live() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;
    let board = scratch.store.board(&workspace).await.unwrap().unwrap();

    // An unblocked rule names no column and a status rule must. The schema
    // holds that line, so no code path can write a rule that fires on nothing.
    scratch
        .store
        .create_mail_rule(
            &board.id,
            &Trigger::Unblocked,
            "You can start now",
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .expect("an unblocked rule is whole without a column");

    let written = scratch.store.mail_rules(&board.id).await.unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].trigger, Trigger::Unblocked);
    assert!(written[0].enabled, "a new rule is live");
}

#[tokio::test]
async fn a_decision_is_written_once_per_rule_and_event() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "CLI install script",
        None,
        &admin,
    )
    .await;
    let rule = a_rule(&scratch.store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(&scratch.store, &workspace, &task, "Backlog", "Done", &admin).await;
    let conn = raw_conn(&scratch).await;
    conn.execute("PRAGMA foreign_keys = ON", ()).await.unwrap();

    conn.execute(
        "INSERT INTO mail_decision (id, rule_id, event_id, task_id, outcome, detail, created_at) \
         VALUES ('d1', ?1, ?2, ?3, 'owed', '', '2026-08-26')",
        turso::params![rule.id.clone(), transition.id.clone(), task.clone()],
    )
    .await
    .unwrap();

    // Same rule, same event, same task: a retry, and the unique index is what
    // makes the retry land on the row that is already there instead of beside
    // it.
    let duplicate = conn
        .execute(
            "INSERT INTO mail_decision (id, rule_id, event_id, task_id, outcome, detail, \
             created_at) VALUES ('d2', ?1, ?2, ?3, 'already_owed', '', '2026-08-26')",
            turso::params![rule.id.clone(), transition.id.clone(), task.clone()],
        )
        .await;
    assert!(duplicate.is_err(), "a duplicate decision was stored");

    let bad_outcome = conn
        .execute(
            "INSERT INTO mail_decision (id, rule_id, event_id, task_id, outcome, detail, \
             created_at) VALUES ('d3', ?1, 'other-event', ?2, 'confused', '', '2026-08-26')",
            turso::params![rule.id.clone(), task.clone()],
        )
        .await;
    assert!(bad_outcome.is_err(), "an unknown outcome was stored");
}

#[tokio::test]
async fn the_index_decides_who_owns_a_send() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "CLI install script",
        None,
        &admin,
    )
    .await;
    let rule = a_rule(&scratch.store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(&scratch.store, &workspace, &task, "Backlog", "Done", &admin).await;

    let now = OffsetDateTime::now_utc();
    let first = scratch
        .store
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now, now)
        .await
        .unwrap();
    assert!(first.is_some(), "the first run owns the send");

    // The engine running a second time over the same crossing — a restart, a
    // retry sweep, two workers — must not mail Ada twice. Nothing is read
    // first: the insert loses.
    let second = scratch
        .store
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now, now)
        .await
        .unwrap();
    assert!(second.is_none(), "the second run owns nothing");

    let ledger = scratch.store.sends_for_rule(&rule.id, 10).await.unwrap();
    assert_eq!(ledger.len(), 1, "one row, one mail");
}

#[tokio::test]
async fn a_second_crossing_is_a_second_send() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship it",
        None,
        &admin,
    )
    .await;
    let rule = a_rule(&scratch.store, &workspace, "Done", "Task completed").await;

    // Done, back to Review, Done again. That is two crossings into Done, and
    // the person is owed two mails: the transition is the event, not the
    // column the card happens to be sitting in.
    let first = moved_to(&scratch.store, &workspace, &task, "Backlog", "Done", &admin).await;
    let _back = moved_to(&scratch.store, &workspace, &task, "Done", "Review", &admin).await;
    let again = moved_to(&scratch.store, &workspace, &task, "Review", "Done", &admin).await;
    assert_ne!(first.id, again.id);

    let now = OffsetDateTime::now_utc();
    for transition in [&first, &again] {
        assert!(
            scratch
                .store
                .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now, now)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(
        scratch
            .store
            .sends_for_rule(&rule.id, 10)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn a_refused_send_is_recorded_and_owed_again() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship it",
        None,
        &admin,
    )
    .await;
    let rule = a_rule(&scratch.store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(&scratch.store, &workspace, &task, "Backlog", "Done", &admin).await;
    let now = OffsetDateTime::now_utc();
    let send = scratch
        .store
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(send.state, SendState::Pending);

    // A connection that timed out says nothing about the address, so it is
    // owed again — and the answer the server gave is kept either way.
    scratch
        .store
        .record_send_refused(
            &send.id,
            "connection timed out",
            Some(now + Duration::minutes(5)),
            now,
        )
        .await
        .unwrap();
    let after = &scratch.store.sends_for_rule(&rule.id, 10).await.unwrap()[0];
    assert_eq!(after.state, SendState::Failed);
    assert_eq!(after.attempts, 1);
    assert_eq!(after.last_error.as_deref(), Some("connection timed out"));

    assert!(
        scratch.store.sends_owed(now, 10).await.unwrap().is_empty(),
        "not owed until it is due"
    );
    let owed = scratch
        .store
        .sends_owed(now + Duration::minutes(6), 10)
        .await
        .unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].id, send.id);
}

#[tokio::test]
async fn a_refused_address_is_not_retried_forever() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship it",
        None,
        &admin,
    )
    .await;
    let rule = a_rule(&scratch.store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(&scratch.store, &workspace, &task, "Backlog", "Done", &admin).await;
    let now = OffsetDateTime::now_utc();
    let send = scratch
        .store
        .claim_send(&rule.id, &transition.id, &task, "gone@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();

    // "No such mailbox" will be just as true tomorrow. It is written down and
    // never offered again.
    scratch
        .store
        .record_send_refused(&send.id, "550 no such mailbox", None, now)
        .await
        .unwrap();
    let after = &scratch.store.sends_for_rule(&rule.id, 10).await.unwrap()[0];
    assert_eq!(after.state, SendState::Abandoned);
    assert_eq!(after.last_error.as_deref(), Some("550 no such mailbox"));
    assert!(
        scratch
            .store
            .sends_owed(now + Duration::days(30), 10)
            .await
            .unwrap()
            .is_empty(),
        "an abandoned send is owed to nobody"
    );
}

#[tokio::test]
async fn an_accepted_send_stops_being_owed() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship it",
        None,
        &admin,
    )
    .await;
    let rule = a_rule(&scratch.store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(&scratch.store, &workspace, &task, "Backlog", "Done", &admin).await;
    let now = OffsetDateTime::now_utc();
    let send = scratch
        .store
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();
    scratch
        .store
        .record_send_accepted(&send.id, now)
        .await
        .unwrap();

    assert!(
        scratch
            .store
            .sends_owed(now + Duration::days(1), 10)
            .await
            .unwrap()
            .is_empty()
    );
    let board = scratch.store.board(&workspace).await.unwrap().unwrap();
    let last = scratch.store.mail_rule_last_sent(&board.id).await.unwrap();
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].0, rule.id);
}

#[tokio::test]
async fn a_viewer_is_never_a_recipient() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let watcher = scratch
        .store
        .create_user(NewUser {
            workspace_id: workspace.clone(),
            email: "viewer@izlek.sh".into(),
            display_name: "Vera".into(),
            role: Role::Viewer,
            invited_by: None,
        })
        .await
        .unwrap();
    let mate = member(&scratch.store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship it",
        None,
        &admin,
    )
    .await;
    scratch.store.assign_task(&task, &mate).await.unwrap();
    // Assigning a Viewer is refused higher up; if a row ever appeared anyway,
    // the store still does not hand the address to the mailer.
    let _ = scratch.store.assign_task(&task, &watcher.id).await;

    let board = scratch.store.board(&workspace).await.unwrap().unwrap();
    for list in [
        scratch.store.recipients_for_task(&task).await.unwrap(),
        scratch.store.recipients_for_board(&board.id).await.unwrap(),
    ] {
        assert!(
            list.iter().all(|person| person.email != "viewer@izlek.sh"),
            "a Viewer was about to be mailed: {list:?}"
        );
    }
    let assignees = scratch.store.recipients_for_task(&task).await.unwrap();
    assert_eq!(
        assignees
            .iter()
            .map(|p| p.email.as_str())
            .collect::<Vec<_>>(),
        ["emre@izlek.sh"]
    );
}

#[tokio::test]
async fn deleting_a_rule_takes_its_ledger_with_it() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship it",
        None,
        &admin,
    )
    .await;
    let rule = a_rule(&scratch.store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(&scratch.store, &workspace, &task, "Backlog", "Done", &admin).await;
    let now = OffsetDateTime::now_utc();
    scratch
        .store
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now, now)
        .await
        .unwrap();

    scratch.store.delete_mail_rule(&rule.id).await.unwrap();
    assert!(
        scratch
            .store
            .sends_for_rule(&rule.id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        scratch.store.delete_mail_rule(&rule.id).await,
        Err(StoreError::NotFound)
    ));
}

// -- the engine ------------------------------------------------------------

use izlek_core::mail::{Engine, MailError, Mailer, Outgoing, Report, backoff};
use std::sync::Mutex;

/// A mail server that remembers instead of sending, and refuses when told to.
struct Remembering {
    sent: Mutex<Vec<Outgoing>>,
    refusals: Mutex<Vec<MailError>>,
}

impl Remembering {
    fn taking_everything() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            refusals: Mutex::new(Vec::new()),
        })
    }

    /// Refuses the next `refusals.len()` attempts, in order, then accepts.
    fn refusing(refusals: Vec<MailError>) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            refusals: Mutex::new(refusals.into_iter().rev().collect()),
        })
    }

    fn sent(&self) -> Vec<Outgoing> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Mailer for Remembering {
    async fn send(&self, mail: &Outgoing) -> Result<(), MailError> {
        if let Some(refusal) = self.refusals.lock().unwrap().pop() {
            return Err(refusal);
        }
        self.sent.lock().unwrap().push(mail.clone());
        Ok(())
    }
}

/// A store shared between the tests and an engine, with no quiet window: a
/// trigger is a mail, sent the moment it is owed. That is what every test
/// below the batching ones is about, and it is what `mail_batch_minutes = 0`
/// means. The batching tests open theirs with `waiting`.
async fn shared() -> (PathBuf, Arc<TursoStore>, String, String) {
    waiting(0).await
}

/// The same, with a quiet window of `minutes` on the workspace.
async fn waiting(minutes: u32) -> (PathBuf, Arc<TursoStore>, String, String) {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(
        TursoStore::open(dir.join("izlek.db").to_str().unwrap())
            .await
            .unwrap(),
    );
    let (workspace, admin) = claim(&store).await;
    let limits = store.workspace().await.unwrap().unwrap();
    store
        .set_limits(
            &workspace,
            limits.attachment_limit_bytes,
            limits.photo_limit_bytes,
            &limits.allowed_file_types,
            minutes,
            15,
        )
        .await
        .unwrap();
    (dir, store, workspace, admin)
}

#[tokio::test]
async fn a_crossing_mails_the_people_on_the_card_once() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(
        &store,
        &workspace,
        "Backlog",
        "CLI install script",
        None,
        &admin,
    )
    .await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;

    let first = engine.on_transition(&transition).await.unwrap();
    assert_eq!(first.sent, 1);
    // The engine running again over the same crossing — a restart, a second
    // worker — owns nothing and sends nothing. The index decided, not a read.
    let second = engine.on_transition(&transition).await.unwrap();
    assert_eq!(second.sent, 0);
    assert_eq!(second.already_owned, 1);

    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "emre@izlek.sh");
    assert_eq!(sent[0].subject, "Task completed");
    assert!(sent[0].body.contains("CLI install script"));
    assert!(
        sent[0].body.contains("https://izlek.sh/?task="),
        "the mail links back to the task: {}",
        sent[0].body
    );
    assert_eq!(store.sends_for_rule(&rule.id, 10).await.unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_task_created_straight_into_a_watched_column_mails_too() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let rule = a_rule(&store, &workspace, "In Progress", "Work started").await;

    let board = store.board(&workspace).await.unwrap().unwrap();
    let column_id = column_named(&store, &workspace, "In Progress").await;
    let created = store
        .create_task(NewTask {
            clock_at: None,
            board_id: &board.id,
            column_id: &column_id,
            parent_id: None,
            title: "Wire the exporter",
            description: "",
            deadline: None,
            created_by: &admin,
        })
        .await
        .unwrap();
    store.assign_task(&created.row.id, &mate).await.unwrap();

    assert_eq!(created.transition.from_column, "");
    assert_eq!(created.transition.to_column, column_id);

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let report = engine.on_transition(&created.transition).await.unwrap();
    assert_eq!(report.sent, 1);

    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "emre@izlek.sh");
    assert_eq!(sent[0].subject, "Work started");
    assert_eq!(store.sends_for_rule(&rule.id, 10).await.unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_edited_rule_does_not_remail_a_crossing_it_already_covered() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;
    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    engine.on_transition(&transition).await.unwrap();

    // Switching a rule off and on again — the shape an edit takes — does not
    // change what it has already covered: the ledger is keyed by the rule and
    // the crossing, so a re-run mails nobody about a crossing from last week.
    store.set_mail_rule_enabled(&rule.id, false).await.unwrap();
    store.set_mail_rule_enabled(&rule.id, true).await.unwrap();
    let again = engine.on_transition(&transition).await.unwrap();
    assert_eq!(again.sent, 0);
    assert_eq!(mailer.sent().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rule_that_is_off_sends_nothing() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;
    store.set_mail_rule_enabled(&rule.id, false).await.unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    assert_eq!(
        engine.on_transition(&transition).await.unwrap(),
        Default::default()
    );
    assert!(mailer.sent().is_empty());
    // Nothing was claimed either: an off rule owes nobody anything, and the
    // ledger must not fill up with rows a later run would skip.
    assert!(store.sends_for_rule(&rule.id, 10).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rule_can_opt_the_task_key_into_its_own_mail() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let with_details = add_task(
        &store,
        &workspace,
        "Backlog",
        "Ship it",
        Some(date!(2026 - 09 - 01)),
        &admin,
    )
    .await;
    store.assign_task(&with_details, &mate).await.unwrap();
    let plain = add_task(&store, &workspace, "Backlog", "Also ship it", None, &admin).await;
    store.assign_task(&plain, &mate).await.unwrap();

    let opted_in = store
        .create_mail_rule(
            &board.id,
            &Trigger::StatusBecomes(Some(column_named(&store, &workspace, "Done").await)),
            "Task completed, with details",
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            true,
        )
        .await
        .unwrap();
    let opted_out = a_rule(&store, &workspace, "Done", "Task completed, plain").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");

    let moved = moved_to(&store, &workspace, &with_details, "Backlog", "Done", &admin).await;
    engine.on_transition(&moved).await.unwrap();
    let moved_plain = moved_to(&store, &workspace, &plain, "Backlog", "Done", &admin).await;
    engine.on_transition(&moved_plain).await.unwrap();

    let sent = mailer.sent();
    let with_details_mail = sent
        .iter()
        .find(|mail| mail.subject == "Task completed, with details")
        .expect("the opted-in rule sent its mail");
    assert!(
        with_details_mail.body.contains("Key:"),
        "an opted-in rule's mail carries the task details block: {}",
        with_details_mail.body
    );
    assert!(with_details_mail.body.contains("Assignees: Emre"));
    assert!(with_details_mail.body.contains("Deadline: 2026-09-01"));

    let plain_mail = sent
        .iter()
        .find(|mail| mail.subject == "Task completed, plain")
        .expect("the opted-out rule also sent its mail");
    assert!(
        !plain_mail.body.contains("Key:"),
        "a rule that never opted in gets no details block: {}",
        plain_mail.body
    );
    let _ = (opted_in, opted_out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_mail_says_when_the_card_moved_not_when_it_was_sent() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    a_rule(&store, &workspace, "Done", "Task completed").await;

    // The server is down when the card moves and comes back later. The mail
    // that finally goes out has to say Tuesday, not Thursday.
    let mailer = Remembering::refusing(vec![MailError::retryable("connection refused")]);
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    let refused = engine.on_transition(&transition).await.unwrap();
    assert_eq!(refused.failed, 1);
    assert!(mailer.sent().is_empty());

    let later = OffsetDateTime::now_utc() + Duration::hours(2);
    let delivered = engine.deliver_owed(later, 10).await.unwrap();
    assert_eq!(delivered.sent, 1);
    let body = mailer.sent()[0].body.clone();
    let moved_day = transition.at.day();
    assert!(
        body.contains(&format!("{moved_day} at")),
        "the mail says when the card moved: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_failing_send_is_retried_a_bounded_number_of_times_and_then_written_off() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;

    let mailer = Remembering::refusing(
        (0..izlek_core::mail::MAX_ATTEMPTS)
            .map(|_| MailError::retryable("connection timed out"))
            .collect(),
    );
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    engine.on_transition(&transition).await.unwrap();

    let mut when = OffsetDateTime::now_utc();
    for _ in 1..izlek_core::mail::MAX_ATTEMPTS {
        when += backoff(izlek_core::mail::MAX_ATTEMPTS) + Duration::minutes(1);
        engine.deliver_owed(when, 10).await.unwrap();
    }

    let ledger = &store.sends_for_rule(&rule.id, 10).await.unwrap()[0];
    assert_eq!(ledger.attempts, izlek_core::mail::MAX_ATTEMPTS);
    assert_eq!(ledger.state, SendState::Abandoned);
    assert_eq!(ledger.last_error.as_deref(), Some("connection timed out"));
    assert!(
        store
            .sends_owed(when + Duration::days(7), 10)
            .await
            .unwrap()
            .is_empty(),
        "a written-off send is not owed again"
    );
    assert!(mailer.sent().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_refused_address_is_written_off_at_the_first_answer() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;

    let mailer = Remembering::refusing(vec![MailError::permanent("550 no such mailbox")]);
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    let report = engine.on_transition(&transition).await.unwrap();
    assert_eq!(report.abandoned, 1);

    let ledger = &store.sends_for_rule(&rule.id, 10).await.unwrap()[0];
    assert_eq!(ledger.state, SendState::Abandoned);
    assert_eq!(ledger.attempts, 1, "a refused address is asked once");
    assert_eq!(ledger.last_error.as_deref(), Some("550 no such mailbox"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn you_can_start_now_waits_for_the_last_blocker() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let waiting = add_task(
        &store,
        &workspace,
        "Backlog",
        "Onboarding mails",
        None,
        &admin,
    )
    .await;
    let first = add_task(&store, &workspace, "Backlog", "Signing keys", None, &admin).await;
    let second = add_task(
        &store,
        &workspace,
        "Backlog",
        "Install script",
        None,
        &admin,
    )
    .await;
    store.assign_task(&waiting, &mate).await.unwrap();
    let now = OffsetDateTime::now_utc();
    store.add_dependency(&waiting, &first, now).await.unwrap();
    store.add_dependency(&waiting, &second, now).await.unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::Unblocked,
            "You can start now",
            Audience::Assignees,
            now,
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");

    // One blocker finished is not unblocked: the other one is still in the way,
    // and telling Emre to start would be a lie.
    let one = moved_to(&store, &workspace, &first, "Backlog", "Done", &admin).await;
    assert_eq!(engine.on_transition(&one).await.unwrap().sent, 0);
    assert!(mailer.sent().is_empty());

    let two = moved_to(&store, &workspace, &second, "Backlog", "Done", &admin).await;
    assert_eq!(engine.on_transition(&two).await.unwrap().sent, 1);
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "emre@izlek.sh");
    assert!(
        sent[0].body.contains("Onboarding mails"),
        "the mail is about the freed task, not the blocker: {}",
        sent[0].body
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_card_that_did_not_move_owes_nobody_a_mail() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    a_rule(&store, &workspace, "Done", "Task completed").await;

    // Dropped back where it came from. There is no transition to hand the
    // engine, and that is the point: nothing recomputed from the card's column
    // could tell the difference.
    let backlog = column_named(&store, &workspace, "Backlog").await;
    let outcome = store
        .move_task(&task, &backlog, &backlog, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(outcome, Moved::Unchanged);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_deleted_blocker_also_says_you_can_start_now() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let waiting = add_task(
        &store,
        &workspace,
        "Backlog",
        "Onboarding mails",
        None,
        &admin,
    )
    .await;
    let blocker = add_task(&store, &workspace, "Backlog", "Signing keys", None, &admin).await;
    store.assign_task(&waiting, &mate).await.unwrap();
    let now = OffsetDateTime::now_utc();
    store.add_dependency(&waiting, &blocker, now).await.unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::Unblocked,
            "You can start now",
            Audience::Assignees,
            now,
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");

    // A blocker can leave the way in two ways, and being deleted is one of
    // them. The person waiting hears about it either way.
    let deletion = store.delete_task(&blocker, &admin, now).await.unwrap();
    assert_eq!(deletion.freed, vec![waiting.clone()]);
    let freeing = deletion.event.clone().expect("the freeing is a fact");
    let first = engine.on_freeing(&freeing, &deletion.freed).await.unwrap();
    assert_eq!(first.sent, 1);

    // And the same freeing processed twice mails once: the unique index is
    // what decides, exactly as it does for a crossing.
    let again = engine.on_freeing(&freeing, &deletion.freed).await.unwrap();
    assert_eq!(again.sent, 0);
    assert_eq!(again.already_owned, 1);

    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "emre@izlek.sh");
    assert_eq!(sent[0].subject, "You can start now");
    assert!(
        sent[0].body.contains("Onboarding mails"),
        "the mail is about the freed task: {}",
        sent[0].body
    );
    assert!(
        sent[0].body.contains("Signing keys"),
        "and it names the blocker that went away: {}",
        sent[0].body
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_delete_that_leaves_somebody_still_waiting_mails_nobody() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let waiting = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let first = add_task(&store, &workspace, "Backlog", "Signing keys", None, &admin).await;
    let second = add_task(
        &store,
        &workspace,
        "Backlog",
        "Install script",
        None,
        &admin,
    )
    .await;
    store.assign_task(&waiting, &mate).await.unwrap();
    let now = OffsetDateTime::now_utc();
    store.add_dependency(&waiting, &first, now).await.unwrap();
    store.add_dependency(&waiting, &second, now).await.unwrap();
    let rule = store
        .create_mail_rule(
            &board.id,
            &Trigger::Unblocked,
            "You can start now",
            Audience::Assignees,
            now,
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");

    // Deleting one of two blockers frees nobody, so there is no event at all
    // and nothing to fire on.
    let deletion = store.delete_task(&first, &admin, now).await.unwrap();
    assert!(deletion.freed.is_empty());
    assert!(deletion.event.is_none());
    assert!(mailer.sent().is_empty());

    // Deleting the second one does free it.
    let deletion = store.delete_task(&second, &admin, now).await.unwrap();
    let freeing = deletion.event.clone().expect("the freeing is a fact");
    assert_eq!(
        engine
            .on_freeing(&freeing, &deletion.freed)
            .await
            .unwrap()
            .sent,
        1
    );
    assert_eq!(store.sends_for_rule(&rule.id, 10).await.unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_mail_owed_by_a_delete_is_rebuilt_from_the_delete_on_a_retry() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let waiting = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let blocker = add_task(&store, &workspace, "Backlog", "Signing keys", None, &admin).await;
    store.assign_task(&waiting, &mate).await.unwrap();
    let now = OffsetDateTime::now_utc();
    store.add_dependency(&waiting, &blocker, now).await.unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::Unblocked,
            "You can start now",
            Audience::Assignees,
            now,
            false,
        )
        .await
        .unwrap();

    // The mail server is down when the blocker is deleted. The sweep picks the
    // send up later with only the ledger row to go on, so the freeing has to be
    // readable back as an event — the deleted task's key included, since the
    // task itself is gone.
    let mailer = Remembering::refusing(vec![MailError::retryable("connection refused")]);
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let deletion = store.delete_task(&blocker, &admin, now).await.unwrap();
    let freeing = deletion.event.clone().unwrap();
    assert_eq!(
        engine
            .on_freeing(&freeing, &deletion.freed)
            .await
            .unwrap()
            .failed,
        1
    );
    assert!(mailer.sent().is_empty());

    let later = OffsetDateTime::now_utc() + Duration::hours(2);
    assert_eq!(engine.deliver_owed(later, 10).await.unwrap().sent, 1);
    let body = mailer.sent()[0].body.clone();
    assert!(
        body.contains("Signing keys"),
        "the retry still names the task that was deleted: {body}"
    );
    assert!(
        body.contains(&format!("{} at", freeing.at.day())),
        "and says when the delete happened, not when the mail went: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nobody_is_mailed_about_what_they_did_themselves() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");

    // Emre is the only assignee and Emre moved the card. Telling him what he
    // just did is how a person learns to filter İzlek's mail away.
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &mate).await;
    let report = engine.on_transition(&transition).await.unwrap();
    assert_eq!(report, Default::default());
    assert!(mailer.sent().is_empty());
    // And nothing is owed: an audience that empties out leaves no ledger row,
    // so the admin's trail does not show a send that never was.
    assert!(store.sends_for_rule(&rule.id, 10).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rule_that_owed_nobody_still_says_so() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &mate).await;
    engine.on_transition(&transition).await.unwrap();

    let decisions = store
        .recent_mail_decisions(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let row = decisions
        .iter()
        .find(|d| d.rule_id == rule.id && d.task_id == task)
        .expect("the empty audience left a row");
    assert_eq!(row.outcome, MailOutcome::NoRecipients);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rule_that_did_not_match_leaves_a_reason() {
    let (dir, store, workspace, admin) = shared().await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    // Rule watches "Done"; this crossing lands in "Review", so the rule never
    // fires for it.
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Review", &admin).await;
    engine.on_transition(&transition).await.unwrap();

    let decisions = store
        .recent_mail_decisions(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let row = decisions
        .iter()
        .find(|d| d.rule_id == rule.id && d.task_id == task)
        .expect("the mismatched trigger left a row");
    assert_eq!(row.outcome, MailOutcome::NotMatched);
    assert!(!row.detail.is_empty(), "the reason is not left blank");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_deleted_task_still_leaves_a_task_gone_row() {
    let (dir, store, workspace, admin) = shared().await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    // The crossing happened; the task is gone before the engine gets to it —
    // a retry, or a worker that lagged behind a delete.
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    store
        .delete_task(&task, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();

    let report = engine.on_transition(&transition).await.unwrap();
    assert_eq!(report, Default::default());
    assert!(mailer.sent().is_empty());

    let decisions = store
        .recent_mail_decisions(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let row = decisions
        .iter()
        .find(|d| d.rule_id == rule.id && d.task_id == task)
        .expect("the deleted task still left a row");
    assert_eq!(row.outcome, MailOutcome::TaskGone);
    // The outcome IS the reason; task_gone carries no detail token.
    assert!(row.detail.is_empty(), "task_gone stores no detail token");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_actor_comes_off_the_board_audience_too_and_the_rest_still_get_it() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let column_id = column_named(&store, &workspace, "Done").await;
    store
        .create_mail_rule(
            &board.id,
            &Trigger::StatusBecomes(Some(column_id)),
            "Task completed",
            Audience::Board,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &mate).await;
    assert_eq!(engine.on_transition(&transition).await.unwrap().sent, 1);

    let sent = mailer.sent();
    assert_eq!(sent.len(), 1, "the board minus the person who moved it");
    assert_eq!(sent[0].to, "ada@izlek.sh");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rule_round_trips_every_word_the_board_speaks() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;
    let board = scratch.store.board(&workspace).await.unwrap().unwrap();

    let triggers = [
        Trigger::Created,
        Trigger::Assigned,
        Trigger::Unassigned,
        Trigger::Commented,
        Trigger::DeadlineSet,
        Trigger::DeadlineCleared,
        Trigger::Retitled,
        Trigger::Linked,
        Trigger::Unlinked,
        Trigger::Deleted,
    ];
    for trigger in triggers {
        let rule = scratch
            .store
            .create_mail_rule(
                &board.id,
                &trigger,
                "A word the board speaks",
                Audience::Assignees,
                OffsetDateTime::now_utc(),
                false,
            )
            .await
            .unwrap();
        let reread = scratch
            .store
            .mail_rules(&board.id)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == rule.id)
            .expect("the rule survived a reread");
        assert_eq!(reread.trigger, trigger, "trigger did not round-trip");
    }
}

#[tokio::test]
async fn updating_a_rule_leaves_its_identity_and_ledger_alone() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship it",
        None,
        &admin,
    )
    .await;
    let rule = a_rule(&scratch.store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(&scratch.store, &workspace, &task, "Backlog", "Done", &admin).await;
    let now = OffsetDateTime::now_utc();
    scratch
        .store
        .record_mail_decision(&rule.id, &transition.id, &task, MailOutcome::Owed, "", now)
        .await
        .unwrap();

    scratch
        .store
        .update_mail_rule(
            &rule.id,
            &Trigger::Unblocked,
            "New subject",
            Audience::Board,
            false,
        )
        .await
        .unwrap();

    let updated = scratch.store.mail_rule(&rule.id).await.unwrap().unwrap();
    assert_eq!(updated.id, rule.id);
    assert_eq!(updated.trigger, Trigger::Unblocked);
    assert_eq!(updated.subject, "New subject");
    assert_eq!(updated.audience, Audience::Board);
    assert_eq!(updated.enabled, rule.enabled);
    assert_eq!(updated.created_at, rule.created_at);

    let decisions = scratch
        .store
        .recent_mail_decisions(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    assert!(
        decisions.iter().any(|d| d.rule_id == rule.id),
        "the pre-existing decision no longer joins to the rule"
    );

    assert!(matches!(
        scratch
            .store
            .update_mail_rule(
                "no-such-rule",
                &Trigger::Unblocked,
                "x",
                Audience::Board,
                false
            )
            .await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_rules_creator_audience_names_who_opened_the_card() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let watcher = scratch
        .store
        .create_user(NewUser {
            workspace_id: workspace.clone(),
            email: "viewer@izlek.sh".into(),
            display_name: "Vera".into(),
            role: Role::Viewer,
            invited_by: None,
        })
        .await
        .unwrap();
    let task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Ship it",
        None,
        &admin,
    )
    .await;
    let creators = scratch
        .store
        .recipients_for_task_creator(&task)
        .await
        .unwrap();
    assert_eq!(
        creators
            .iter()
            .map(|p| p.user_id.as_str())
            .collect::<Vec<_>>(),
        [admin.as_str()]
    );

    let watcher_task = add_task(
        &scratch.store,
        &workspace,
        "Backlog",
        "Watched",
        None,
        &watcher.id,
    )
    .await;
    assert!(
        scratch
            .store
            .recipients_for_task_creator(&watcher_task)
            .await
            .unwrap()
            .is_empty(),
        "a Viewer creator was about to be mailed"
    );
}

/// Who made an account is recorded when it is made. The first account was made
/// by nobody, and says so rather than pointing at itself.
#[tokio::test]
async fn an_account_records_the_admin_who_made_it() {
    let (scratch, ws_id, admin_id) = workspace_with_admin().await;

    let owner = scratch.store.user(&admin_id).await.unwrap().unwrap();
    assert_eq!(owner.invited_by, None, "the first account named an inviter");

    let member = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id,
            email: "grace@izlek.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
            invited_by: Some(admin_id.clone()),
        })
        .await
        .unwrap();
    assert_eq!(member.invited_by.as_deref(), Some(admin_id.as_str()));

    // And it survives a reread, which is the part a column that is only set in
    // the INSERT would fail.
    let reread = scratch.store.user(&member.id).await.unwrap().unwrap();
    assert_eq!(reread.invited_by.as_deref(), Some(admin_id.as_str()));
}

#[tokio::test]
async fn a_file_goes_into_the_database_file_and_comes_back_byte_for_byte() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();
    let task = add_task(store, &workspace, "Backlog", "a task", None, &admin).await;

    // Bytes that are not text and are not valid UTF-8, because an attachment
    // is not a string and the column it lives in must not treat it as one.
    let bytes: Vec<u8> = vec![
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00, 0xfe,
    ];
    let id = store
        .add_attachment(NewAttachment {
            task_id: &task,
            comment_id: None,
            file_name: "shot.png",
            mime_type: "image/png",
            bytes: bytes.clone(),
            uploaded_by: &admin,
            at: now,
        })
        .await
        .unwrap();

    let row = store.attachment(&id).await.unwrap().unwrap();
    assert_eq!(row.task_id, task);
    assert_eq!(row.file_name, "shot.png");
    assert_eq!(row.mime_type, "image/png");
    assert_eq!(row.size_bytes, bytes.len() as u64);
    assert_eq!(row.uploaded_by, admin);
    assert_eq!(store.attachment_bytes(&id).await.unwrap().unwrap(), bytes);
}

#[tokio::test]
async fn a_file_name_that_is_a_path_is_stored_as_the_label_it_is() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();
    let task = add_task(store, &workspace, "Backlog", "a task", None, &admin).await;

    // Nothing in the store resolves a name, so the worst name there is comes
    // back exactly as it went in: it is a label on a chip, not a path.
    let id = store
        .add_attachment(NewAttachment {
            task_id: &task,
            comment_id: None,
            file_name: "../../etc/passwd",
            mime_type: "application/octet-stream",
            bytes: b"root:x:0:0".to_vec(),
            uploaded_by: &admin,
            at: now,
        })
        .await
        .unwrap();
    let row = store.attachment(&id).await.unwrap().unwrap();
    assert_eq!(row.file_name, "../../etc/passwd");
    // And nothing was written anywhere but the database file and its key
    // (see `store::secret`) — never a file named after the attachment.
    let written: Vec<String> = std::fs::read_dir(&scratch.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with("izlek.db") && name != "izlek.key")
        .collect();
    assert!(written.is_empty(), "{written:?}");
}

#[tokio::test]
async fn the_detail_lists_a_task_s_files_oldest_first_and_never_their_bytes() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();
    let task = add_task(store, &workspace, "Backlog", "a task", None, &admin).await;

    for (n, name) in ["first.pdf", "second.png"].iter().enumerate() {
        store
            .add_attachment(NewAttachment {
                task_id: &task,
                comment_id: None,
                file_name: name,
                mime_type: "application/pdf",
                bytes: vec![b'x'; 2048],
                uploaded_by: &admin,
                at: now + time::Duration::seconds(n as i64),
            })
            .await
            .unwrap();
    }

    let detail = load_detail(store, &workspace, &task)
        .await
        .unwrap()
        .unwrap();
    let names: Vec<&str> = detail.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["first.pdf", "second.png"]);
    assert_eq!(detail.files[0].size_label(), "2 KB");
}

#[tokio::test]
async fn a_file_can_be_taken_away_and_saying_so_twice_is_answered_honestly() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();
    let task = add_task(store, &workspace, "Backlog", "a task", None, &admin).await;
    let id = store
        .add_attachment(NewAttachment {
            task_id: &task,
            comment_id: None,
            file_name: "note.txt",
            mime_type: "text/plain",
            bytes: b"hello".to_vec(),
            uploaded_by: &admin,
            at: now,
        })
        .await
        .unwrap();

    assert!(store.delete_attachment(&id).await.unwrap());
    assert!(!store.delete_attachment(&id).await.unwrap());
    assert!(store.attachment(&id).await.unwrap().is_none());
    assert!(store.attachment_bytes(&id).await.unwrap().is_none());
    assert!(store.attachments(&task).await.unwrap().is_empty());
}

/// The queue reads soonest-first, ascending; a keyset walk in that same
/// direction covers every owed send exactly once, whatever order the pages
/// come back in.
#[tokio::test]
async fn keyset_paging_covers_the_queue_ascending() {
    use izlek_core::store::{FeedCursor, FeedPage};

    let (scratch, _workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let t0 = OffsetDateTime::now_utc();
    for i in 0..11 {
        store
            .queue_invite(
                &format!("row{i}@izlek.sh"),
                "Join",
                "Come aboard.",
                t0 + Duration::seconds(i),
            )
            .await
            .unwrap();
    }

    let whole = store.mail_queue(100, FeedPage::Newest).await.unwrap();
    assert_eq!(whole.len(), 11);

    let mut walked = Vec::new();
    let mut page = FeedPage::Newest;
    loop {
        let rows = store.mail_queue(4, page).await.unwrap();
        if rows.is_empty() {
            break;
        }
        let last = rows.last().unwrap();
        page = FeedPage::Before(FeedCursor {
            at: last.next_attempt_at.unwrap(),
            id: last.id.clone(),
        });
        walked.extend(rows);
    }
    let walked_ids: Vec<&str> = walked.iter().map(|r| r.id.as_str()).collect();
    let whole_ids: Vec<&str> = whole.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(walked_ids, whole_ids);
}

#[tokio::test]
async fn the_queue_shows_what_is_owed_and_not_what_is_done() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let task = add_task(store, &workspace, "Backlog", "a task", None, &admin).await;
    let rule = a_rule(store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(store, &workspace, &task, "Backlog", "Done", &admin).await;
    let now = OffsetDateTime::now_utc();

    let pending = store
        .claim_send(
            &rule.id,
            &transition.id,
            &task,
            "pending@izlek.sh",
            now,
            now,
        )
        .await
        .unwrap()
        .unwrap();

    let failed = store
        .claim_send(&rule.id, &transition.id, &task, "failed@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();
    store
        .record_send_refused(&failed.id, "timeout", Some(now + Duration::minutes(5)), now)
        .await
        .unwrap();

    let sent = store
        .claim_send(&rule.id, &transition.id, &task, "sent@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();
    store.record_send_accepted(&sent.id, now).await.unwrap();

    let abandoned = store
        .claim_send(
            &rule.id,
            &transition.id,
            &task,
            "abandoned@izlek.sh",
            now,
            now,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .record_send_refused(&abandoned.id, "bounced", None, now)
        .await
        .unwrap();

    let queue = store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let ids: Vec<&str> = queue.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&pending.id.as_str()));
    assert!(ids.contains(&failed.id.as_str()));
    assert!(!ids.contains(&sent.id.as_str()));
    assert!(!ids.contains(&abandoned.id.as_str()));
}

#[tokio::test]
async fn an_invite_mail_is_owed_without_a_rule() {
    let (scratch, _workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let invite = store
        .queue_invite("newcomer@izlek.sh", "Join İzlek", "Come aboard.", now)
        .await
        .unwrap();
    assert_eq!(invite.rule_id, None);
    assert_eq!(invite.kind, SendKind::Invite);

    let owed = store.sends_owed(now, 10).await.unwrap();
    let found = owed.iter().find(|s| s.id == invite.id).unwrap();
    assert_eq!(found.rule_id, None);

    let queue = store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let found = queue.iter().find(|s| s.id == invite.id).unwrap();
    assert_eq!(found.rule_id, None);
}

#[tokio::test]
async fn an_invite_mail_with_no_sender_is_held_not_failed() {
    let (dir, store, _workspace, _admin) = shared().await;
    let now = OffsetDateTime::now_utc();
    store
        .queue_invite("newcomer@izlek.sh", "Join İzlek", "Come aboard.", now)
        .await
        .unwrap();

    let mailer = Remembering::refusing(vec![MailError::unsent("no sender configured")]);
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let report = engine.deliver_owed(now, 10).await.unwrap();
    assert_eq!(report.held, 1);
    assert_eq!(report.sent, 0);

    let queue = store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let held = queue
        .iter()
        .find(|s| s.recipient == "newcomer@izlek.sh")
        .unwrap();
    assert_eq!(held.attempts, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A mailer that takes its time, so a second pass has somewhere to arrive.
struct Dawdling {
    sent: Mutex<Vec<Outgoing>>,
}

#[async_trait::async_trait]
impl Mailer for Dawdling {
    async fn send(&self, mail: &Outgoing) -> Result<(), MailError> {
        // The window the bug lived in: the row still looks owed for as long as
        // the mail server is thinking about it.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        self.sent.lock().unwrap().push(mail.clone());
        Ok(())
    }
}

#[tokio::test]
async fn two_passes_over_one_owed_mail_send_it_once() {
    // Queueing an invite both spawns a delivery pass off the request and wakes
    // the sweep, so two passes read the ledger at the same moment. Owed is not
    // ownership: whoever loses the claim must not also send. Three invited
    // members once arrived as six mails.
    let (dir, store, _workspace, _admin) = shared().await;
    let now = OffsetDateTime::now_utc();
    store
        .queue_invite("newcomer@izlek.sh", "Join İzlek", "Come aboard.", now)
        .await
        .unwrap();

    let mailer = Arc::new(Dawdling {
        sent: Mutex::new(Vec::new()),
    });
    // Two engines over one store, which is what the process actually has: the
    // request's pass and the sweep's.
    let request = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let sweep = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let (a, b) = tokio::join!(request.deliver_owed(now, 10), sweep.deliver_owed(now, 10),);
    let (a, b) = (a.unwrap(), b.unwrap());

    let sent = mailer.sent.lock().unwrap().len();
    assert_eq!(
        sent, 1,
        "one queued invite left as {sent} mails: the loser of the claim sent it too"
    );
    assert_eq!(
        a.sent + b.sent,
        1,
        "both passes reported sending the same mail"
    );

    // The winner wrote down that it went, so the row does not come back when
    // the lease runs out — the loser leaving it alone is not the same as the
    // mail being dropped.
    let owed = store
        .sends_owed(now + izlek_core::mail::LEASE + Duration::seconds(1), 10)
        .await
        .unwrap();
    assert!(
        owed.is_empty(),
        "a mail that was sent is owed again once the lease expires"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_crossing_and_a_sweep_at_the_same_moment_mail_once() {
    // The unique index stops a crossing being *owed* twice. It does nothing
    // about the one row being *sent* twice: writing it announces the queue,
    // which wakes the sweep, while the pass that wrote it is still composing.
    // The row is therefore born held, and this is the test that says so.
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let _rule = a_rule(&store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;

    let mailer = Arc::new(Dawdling {
        sent: Mutex::new(Vec::new()),
    });
    let crossing = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let sweep = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");

    // The sweep passes run while the crossing is inside its send, which is
    // exactly when the row it just wrote is sitting in the ledger.
    let (crossed, _swept) = tokio::join!(crossing.on_transition(&transition), async {
        for _ in 0..4 {
            let _ = sweep
                .deliver_owed(OffsetDateTime::now_utc(), 10)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
    });
    crossed.unwrap();

    let sent = mailer.sent.lock().unwrap().len();
    assert_eq!(
        sent, 1,
        "one crossing left as {sent} mails: the sweep sent the row the crossing was sending"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_lease_that_expires_gives_the_mail_back() {
    // The claim is a lease, not a deletion: a process that dies between taking
    // a row and writing down what happened must not take the mail with it.
    let (scratch, _workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();
    let invite = store
        .queue_invite("newcomer@izlek.sh", "Join İzlek", "Come aboard.", now)
        .await
        .unwrap();

    let mine = store
        .claim_sends_owed(now, now + izlek_core::mail::LEASE, 10)
        .await
        .unwrap();
    assert!(
        mine.iter().any(|s| s.id == invite.id),
        "the first pass did not get a mail nobody holds"
    );
    let theirs = store
        .claim_sends_owed(now, now + izlek_core::mail::LEASE, 10)
        .await
        .unwrap();
    assert!(
        !theirs.iter().any(|s| s.id == invite.id),
        "a second pass was handed a mail that was already held"
    );
    // Nobody wrote the outcome down, so once the lease is out the mail is owed
    // again rather than lost.
    let owed = store
        .sends_owed(now + izlek_core::mail::LEASE + Duration::seconds(1), 10)
        .await
        .unwrap();
    assert!(
        owed.iter().any(|s| s.id == invite.id),
        "an abandoned lease swallowed the mail"
    );
}

#[tokio::test]
async fn the_activity_feed_is_the_whole_workspace_newest_first() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let other = member(store, &workspace, "sam@izlek.sh", "Sam").await;
    let task_a = add_task(store, &workspace, "Backlog", "first task", None, &admin).await;
    let task_b = add_task(store, &workspace, "Backlog", "second task", None, &admin).await;
    let t0 = OffsetDateTime::now_utc();

    store
        .record_activity(&task_a, Some(&admin), None, &ActivityKind::Created, "", t0)
        .await
        .unwrap();
    store
        .record_activity(
            &task_b,
            Some(&other),
            None,
            &ActivityKind::Retitled,
            "new title",
            t0 + Duration::seconds(1),
        )
        .await
        .unwrap();

    let feed = store
        .recent_activity(
            10,
            izlek_core::store::FeedPage::Newest,
            izlek_core::store::Dir::Newest,
            &izlek_core::store::ActivityFilter::default(),
        )
        .await
        .unwrap();
    // Two "created" lines came free with the two tasks; the two just recorded
    // sit newest first, ahead of both of those.
    assert_eq!(feed.len(), 4);
    assert_eq!(feed[0].task_id.as_deref(), Some(task_b.as_str()));
    assert_eq!(feed[0].title.as_deref(), Some("second task"));
    assert_eq!(feed[0].actor_name.as_deref(), Some("Sam"));
    assert_eq!(feed[0].kind, ActivityKind::Retitled);
    assert_eq!(feed[1].task_id.as_deref(), Some(task_a.as_str()));
    assert_eq!(feed[1].actor_name.as_deref(), Some("Ada"));
}

/// A keyset page never skips or repeats a row even though the underlying
/// rows never move: walking `Before` from the top to the bottom, one page at
/// a time, visits every row exactly once, in the same order the unpaged
/// feed reads.
#[tokio::test]
async fn keyset_paging_covers_every_activity_row_once() {
    use izlek_core::store::{FeedCursor, FeedPage};

    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let t0 = OffsetDateTime::now_utc();
    for i in 0..23 {
        store
            .record_event(
                Some(&admin),
                &ActivityKind::Other("row".to_string()),
                &format!("row {i}"),
                t0 + Duration::seconds(i),
            )
            .await
            .unwrap();
    }
    let _ = workspace;

    let whole = store
        .recent_activity(
            100,
            FeedPage::Newest,
            izlek_core::store::Dir::Newest,
            &izlek_core::store::ActivityFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(whole.len(), 23);

    let mut walked = Vec::new();
    let mut page = FeedPage::Newest;
    loop {
        let rows = store
            .recent_activity(
                7,
                page,
                izlek_core::store::Dir::Newest,
                &izlek_core::store::ActivityFilter::default(),
            )
            .await
            .unwrap();
        if rows.is_empty() {
            break;
        }
        let last = rows.last().unwrap();
        page = FeedPage::Before(FeedCursor {
            at: last.at,
            id: last.id.clone(),
        });
        walked.extend(rows);
    }
    let walked_ids: Vec<&str> = walked.iter().map(|r| r.id.as_str()).collect();
    let whole_ids: Vec<&str> = whole.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(walked_ids, whole_ids);
}

#[tokio::test]
async fn an_account_event_rides_the_feed_without_a_task() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let task = add_task(store, &workspace, "Backlog", "a task", None, &admin).await;
    let t0 = OffsetDateTime::now_utc();

    store
        .record_activity(&task, Some(&admin), None, &ActivityKind::Created, "", t0)
        .await
        .unwrap();
    store
        .record_event(
            Some(&admin),
            &ActivityKind::SignedIn,
            "from 198.51.100.7",
            t0 + Duration::seconds(1),
        )
        .await
        .unwrap();

    let feed = store
        .recent_activity(
            10,
            izlek_core::store::FeedPage::Newest,
            izlek_core::store::Dir::Newest,
            &izlek_core::store::ActivityFilter::default(),
        )
        .await
        .unwrap();
    // The sign-in sits newest with no task on it; the task's own lines below
    // still name theirs.
    assert_eq!(feed[0].kind, ActivityKind::SignedIn);
    assert_eq!(feed[0].task_id, None);
    assert_eq!(feed[0].title, None);
    assert_eq!(feed[0].actor_name.as_deref(), Some("Ada"));
    assert_eq!(feed[0].detail, "from 198.51.100.7");
    assert!(
        feed.iter()
            .any(|line| line.task_id.as_deref() == Some(task.as_str())),
        "the task's own lines kept their task"
    );
}

#[tokio::test]
async fn a_comment_leaves_one_activity_row_that_resolves_as_an_event() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let task = add_task(store, &workspace, "Backlog", "note me", None, &admin).await;

    let written = store
        .add_comment(&task, &admin, "hello", OffsetDateTime::now_utc())
        .await
        .unwrap();

    let activity = store.activity_for_task(&task).await.unwrap();
    let commented: Vec<_> = activity
        .iter()
        .filter(|line| line.kind == ActivityKind::Commented)
        .collect();
    assert_eq!(commented.len(), 1);
    assert_eq!(commented[0].id, written.activity_id);

    match store.event(&written.activity_id).await.unwrap() {
        Some(Event::Happened(event)) => {
            assert_eq!(event.task_id, task);
            assert_eq!(event.actor_id, admin);
            assert_eq!(event.kind, ActivityKind::Commented);
        }
        other => panic!("expected Event::Happened, got {other:?}"),
    }
}

#[tokio::test]
async fn a_created_task_s_activity_id_resolves_as_an_event() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let column_id = column_named(store, &workspace, "Backlog").await;

    let created = store
        .create_task(NewTask {
            clock_at: None,
            parent_id: None,
            board_id: &board.id,
            column_id: &column_id,
            title: "brand new",
            description: "",
            deadline: None,
            created_by: &admin,
        })
        .await
        .unwrap();

    match store.event(&created.activity_id).await.unwrap() {
        Some(Event::Happened(event)) => {
            assert_eq!(event.task_id, created.row.id);
            assert_eq!(event.actor_id, admin);
            assert_eq!(event.kind, ActivityKind::Created);
        }
        other => panic!("expected Event::Happened, got {other:?}"),
    }
}

// -- the engine hears activity ----------------------------------------------

/// Reads an activity row back as the event the engine takes.
async fn activity_event(store: &TursoStore, activity_id: &str) -> izlek_core::store::ActivityEvent {
    match store.event(activity_id).await.unwrap() {
        Some(Event::Happened(event)) => event,
        other => panic!("expected Event::Happened, got {other:?}"),
    }
}

#[tokio::test]
async fn an_assignment_mails_the_assignees_minus_the_actor() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let board = store.board(&workspace).await.unwrap().unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::Assigned,
            "You were assigned",
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    // Ada assigned Emre; Ada is not on the assignee list to begin with, so
    // the audience is Emre alone.
    let activity_id = store
        .record_activity(
            &task,
            Some(&admin),
            None,
            &ActivityKind::Assigned,
            "Emre",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let event = activity_event(&store, &activity_id).await;

    let first = engine.on_activity(&event).await.unwrap();
    assert_eq!(first.sent, 1);
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "emre@izlek.sh");

    // The same activity replayed — a restart, a second worker — owns nothing
    // and sends nothing more. The unique index decided, not a read.
    let second = engine.on_activity(&event).await.unwrap();
    assert_eq!(second.sent, 0);
    assert_eq!(second.already_owned, 1);
    assert_eq!(mailer.sent().len(), 1, "still exactly one mail out");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn assigning_one_person_mails_that_person_not_every_assignee() {
    let (dir, store, workspace, admin) = shared().await;
    let first = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let second = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let third = member(&store, &workspace, "linus@izlek.sh", "Linus").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &first).await.unwrap();
    store.assign_task(&task, &second).await.unwrap();
    store.assign_task(&task, &third).await.unwrap();
    let board = store.board(&workspace).await.unwrap().unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::Assigned,
            "You were assigned",
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    // Ada assigns Yusuf to a card that already carries three people: the line
    // is about Yusuf, so Yusuf is the audience — not the three already there.
    let fourth = member(&store, &workspace, "yusuf@izlek.sh", "Yusuf").await;
    let activity_id = store
        .record_activity(
            &task,
            Some(&admin),
            Some(&fourth),
            &ActivityKind::Assigned,
            "Yusuf",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let event = activity_event(&store, &activity_id).await;

    let report = engine.on_activity(&event).await.unwrap();
    assert_eq!(report.sent, 1, "one assignment, one mail");
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "yusuf@izlek.sh");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_time_change_fires_a_deadline_set_rule() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::DeadlineSet,
            "Moment moved",
            Audience::Board,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    // Ada sets the time on her own card — a ClockSet activity, no deadline
    // in sight — and the only person the board audience can mail is Emre,
    // the board minus the actor.
    let activity_id = store
        .record_activity(
            &task,
            Some(&admin),
            None,
            &ActivityKind::ClockSet,
            "2026-09-02T09:30:00+00:00",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let event = activity_event(&store, &activity_id).await;

    let report = engine.on_activity(&event).await.unwrap();
    assert_eq!(report.sent, 1, "the time change mails the board");
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "emre@izlek.sh");
    assert!(
        sent[0].body.contains("set the time to"),
        "the body says the honest per-kind sentence: {}",
        sent[0].body
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unassignment_mails_the_person_removed_not_the_rest() {
    let (dir, store, workspace, admin) = shared().await;
    let staying = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let leaving = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &staying).await.unwrap();
    store.assign_task(&task, &leaving).await.unwrap();
    let board = store.board(&workspace).await.unwrap().unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::Unassigned,
            "You were unassigned",
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    store.unassign_task(&task, &leaving).await.unwrap();
    let activity_id = store
        .record_activity(
            &task,
            Some(&admin),
            Some(&leaving),
            &ActivityKind::Unassigned,
            "Emre",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let event = activity_event(&store, &activity_id).await;

    let report = engine.on_activity(&event).await.unwrap();
    assert_eq!(report.sent, 1, "one removal, one mail");
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].to, "emre@izlek.sh",
        "the mail is for the one removed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rule_whose_line_has_no_subject_still_mails_every_assignee() {
    let (dir, store, workspace, admin) = shared().await;
    let first = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let second = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &first).await.unwrap();
    store.assign_task(&task, &second).await.unwrap();
    let board = store.board(&workspace).await.unwrap().unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::Commented,
            "There was movement",
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    // A comment has no subject: the audience stays the whole assignee list,
    // minus the actor, exactly as before the subject column existed.
    let activity_id = store
        .record_activity(
            &task,
            Some(&admin),
            None,
            &ActivityKind::Commented,
            "",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let event = activity_event(&store, &activity_id).await;

    let report = engine.on_activity(&event).await.unwrap();
    assert_eq!(report.sent, 2, "both assignees, the old breadth");
    let mut to: Vec<_> = mailer.sent().iter().map(|m| m.to.clone()).collect();
    to.sort();
    assert_eq!(to, vec!["emre@izlek.sh", "grace@izlek.sh"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_creator_audience_rule_mails_the_creator_and_nobody_when_the_creator_commented() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    store
        .create_mail_rule(
            &board.id,
            &Trigger::Commented,
            "A comment landed",
            Audience::Creator,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");

    // Emre comments on Ada's task: Ada, the creator, gets mailed.
    let commented_by_mate = store
        .record_activity(
            &task,
            Some(&mate),
            None,
            &ActivityKind::Commented,
            "",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let event = activity_event(&store, &commented_by_mate).await;
    let report = engine.on_activity(&event).await.unwrap();
    assert_eq!(report.sent, 1);
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "ada@izlek.sh");

    // Ada comments on her own task: the audience is only the actor, so
    // nobody is mailed and the ledger says why rather than looking silent.
    let commented_by_creator = store
        .record_activity(
            &task,
            Some(&admin),
            None,
            &ActivityKind::Commented,
            "",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let event = activity_event(&store, &commented_by_creator).await;
    let report = engine.on_activity(&event).await.unwrap();
    assert_eq!(report, Default::default());
    assert_eq!(mailer.sent().len(), 1, "no second mail went out");

    let decisions = store
        .recent_mail_decisions(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let row = decisions
        .iter()
        .find(|d| d.event_id == commented_by_creator)
        .expect("the creator-only audience left a row");
    assert_eq!(row.outcome, MailOutcome::NoRecipients);
    assert_eq!(row.detail, "actor_only");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_created_activity_does_not_fire_a_status_rule() {
    let (dir, store, workspace, admin) = shared().await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let rule = a_rule(&store, &workspace, "Done", "Task completed").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let activity_id = store
        .record_activity(
            &task,
            Some(&admin),
            None,
            &ActivityKind::Created,
            "",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let event = activity_event(&store, &activity_id).await;

    let report = engine.on_activity(&event).await.unwrap();
    assert_eq!(report, Default::default());
    assert!(mailer.sent().is_empty());

    let decisions = store
        .recent_mail_decisions(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let row = decisions
        .iter()
        .find(|d| d.rule_id == rule.id && d.task_id == task)
        .expect("the mismatched trigger left a row");
    assert_eq!(row.outcome, MailOutcome::NotMatched);
    assert!(!row.detail.is_empty(), "the reason is not left blank");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn decisions_and_sends_for_task_read_only_that_tasks_own_rows() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let task = add_task(store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let other_task = add_task(store, &workspace, "Backlog", "Something else", None, &admin).await;
    let rule = a_rule(store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(store, &workspace, &task, "Backlog", "Done", &admin).await;
    let other_transition =
        moved_to(store, &workspace, &other_task, "Backlog", "Done", &admin).await;
    let now = OffsetDateTime::now_utc();

    store
        .record_mail_decision(&rule.id, &transition.id, &task, MailOutcome::Owed, "", now)
        .await
        .unwrap();
    store
        .record_mail_decision(
            &rule.id,
            &other_transition.id,
            &other_task,
            MailOutcome::Owed,
            "",
            now,
        )
        .await
        .unwrap();
    let send = store
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();
    store
        .claim_send(
            &rule.id,
            &other_transition.id,
            &other_task,
            "emre@izlek.sh",
            now,
            now,
        )
        .await
        .unwrap();

    let decisions = store.decisions_for_task(&task, 10).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].task_id, task);

    let sends = store.sends_for_task(&task, 10).await.unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].id, send.id);
    assert_eq!(sends[0].task_id.as_deref(), Some(task.as_str()));
}

#[tokio::test]
async fn requeuing_a_send_puts_it_back_in_play_but_leaves_a_sent_one_alone() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let task = add_task(store, &workspace, "Backlog", "Ship it", None, &admin).await;
    let rule = a_rule(store, &workspace, "Done", "Task completed").await;
    let transition = moved_to(store, &workspace, &task, "Backlog", "Done", &admin).await;
    let now = OffsetDateTime::now_utc();

    let failed = store
        .claim_send(&rule.id, &transition.id, &task, "failed@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();
    store
        .record_send_refused(&failed.id, "timeout", None, now)
        .await
        .unwrap();

    let sent = store
        .claim_send(&rule.id, &transition.id, &task, "sent@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();
    store.record_send_accepted(&sent.id, now).await.unwrap();

    store.requeue_send(&failed.id, now).await.unwrap();
    store.requeue_send(&sent.id, now).await.unwrap();

    let sends = store.sends_for_task(&task, 10).await.unwrap();
    let reread_failed = sends.iter().find(|s| s.id == failed.id).unwrap();
    assert_eq!(reread_failed.state, SendState::Pending);
    assert!(reread_failed.next_attempt_at.is_some_and(|at| at <= now));

    let reread_sent = sends.iter().find(|s| s.id == sent.id).unwrap();
    assert_eq!(
        reread_sent.state,
        SendState::Sent,
        "a sent send is untouched"
    );

    let owed = store.sends_owed(now, 10).await.unwrap();
    assert!(
        owed.iter().any(|s| s.id == failed.id),
        "the requeued send is now due"
    );
}

#[tokio::test]
async fn a_queued_notice_is_pending_and_due_and_on_the_queue() {
    let scratch = Scratch::open().await;
    let now = OffsetDateTime::now_utc();
    let queued = scratch
        .store
        .queue_notice("grace@izlek.sh", "Heads up", "Body text", now)
        .await
        .unwrap();
    assert_eq!(queued.kind, SendKind::Notice);
    assert_eq!(queued.state, SendState::Pending);
    assert_eq!(queued.recipient, "grace@izlek.sh");
    assert_eq!(queued.subject.as_deref(), Some("Heads up"));
    assert_eq!(queued.body.as_deref(), Some("Body text"));

    let owed = scratch.store.sends_owed(now, 10).await.unwrap();
    assert!(owed.iter().any(|s| s.id == queued.id), "not due: {owed:?}");

    let mail_queue = scratch
        .store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    assert!(
        mail_queue.iter().any(|s| s.id == queued.id),
        "not in mail_queue: {mail_queue:?}"
    );
}

/// An admin's notice carries its own subject and body and owes no rule, the
/// same as an invite — the engine has to deliver it rather than skip past it
/// looking for a rule that was never there.
#[tokio::test]
async fn the_engine_delivers_a_notice_that_owes_no_rule() {
    let (dir, store, _workspace, _admin) = shared().await;
    let now = OffsetDateTime::now_utc();
    store
        .queue_notice("grace@izlek.sh", "Heads up", "Body text", now)
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let report = engine
        .deliver_owed(now + Duration::minutes(1), 10)
        .await
        .unwrap();

    assert_eq!(report.sent, 1, "{report:?}");
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].to, "grace@izlek.sh");
    assert_eq!(sent[0].subject, "Heads up");
    assert_eq!(sent[0].body, "Body text");

    let owed = store
        .sends_owed(now + Duration::hours(2), 10)
        .await
        .unwrap();
    assert!(
        owed.is_empty(),
        "a delivered notice is no longer owed: {owed:?}"
    );
    std::fs::remove_dir_all(dir).ok();
}

/// A task's own history reads newest first, the same way every other feed in
/// the app does — the oldest line is the last one, not the first.
#[tokio::test]
async fn a_tasks_activity_reads_newest_first() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let task = add_task(store, &workspace, "Backlog", "order me", None, &admin).await;

    for word in ["one", "two", "three"] {
        store
            .add_comment(&task, &admin, word, OffsetDateTime::now_utc())
            .await
            .unwrap();
    }

    let activity = store.activity_for_task(&task).await.unwrap();
    let stamps: Vec<_> = activity.iter().map(|line| line.at).collect();
    assert!(
        stamps.windows(2).all(|pair| pair[0] >= pair[1]),
        "not newest first: {stamps:?}"
    );
    // Creation is the oldest thing that ever happens to a task, so it is last
    // now; a comment — the most recent act here — leads.
    assert_eq!(
        activity.first().map(|line| line.kind.clone()),
        Some(ActivityKind::Commented),
        "{activity:?}"
    );
    assert_eq!(
        activity.last().map(|line| line.kind.clone()),
        Some(ActivityKind::Created),
        "{activity:?}"
    );
}

/// A status rule with no column watches the whole board: every crossing fires
/// it, not just the one into a named column.
#[tokio::test]
async fn a_status_rule_with_no_column_fires_on_every_crossing() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();

    store
        .create_mail_rule(
            &board.id,
            &Trigger::StatusBecomes(None),
            "It moved",
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");

    let first = moved_to(&store, &workspace, &task, "Backlog", "In Progress", &admin).await;
    engine.on_transition(&first).await.unwrap();
    let second = moved_to(&store, &workspace, &task, "In Progress", "Done", &admin).await;
    engine.on_transition(&second).await.unwrap();

    let sent = mailer.sent();
    assert_eq!(
        sent.iter()
            .filter(|mail| mail.subject == "It moved")
            .count(),
        2,
        "both crossings fired the every-column rule: {sent:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The every-column rule round-trips through the store: it is written with no
/// column and read back as one, not as a rule pointing at a column named "".
#[tokio::test]
async fn an_every_column_rule_survives_the_round_trip() {
    let (dir, store, workspace, admin) = shared().await;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let rule = store
        .create_mail_rule(
            &board.id,
            &Trigger::StatusBecomes(None),
            "It moved",
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();
    let read = store
        .mail_rules(&board.id)
        .await
        .unwrap()
        .into_iter()
        .find(|stored| stored.id == rule.id)
        .expect("the rule is on the board");
    assert_eq!(read.trigger, Trigger::StatusBecomes(None));
    let _ = admin;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A box behind a proxy answers on one address and is reached on another, so
/// an admin can set the origin mail links point at. What is stored wins over
/// what the process was configured with.
#[tokio::test]
async fn a_stored_address_is_the_one_mail_links_point_at() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    a_rule(&store, &workspace, "Done", "Task completed").await;
    store
        .set_public_url(&workspace, Some("https://board.example"))
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "http://127.0.0.1:3000");
    let moved = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    engine.on_transition(&moved).await.unwrap();

    let sent = mailer.sent();
    assert!(
        sent[0].body.contains("https://board.example/?task="),
        "the mail did not use the stored address: {}",
        sent[0].body
    );

    // Cleared, the configured address is what is left.
    store.set_public_url(&workspace, None).await.unwrap();
    let back = add_task(&store, &workspace, "Backlog", "Ship it too", None, &admin).await;
    store.assign_task(&back, &mate).await.unwrap();
    let moved = moved_to(&store, &workspace, &back, "Backlog", "Done", &admin).await;
    engine.on_transition(&moved).await.unwrap();
    let sent = mailer.sent();
    assert!(
        sent.last()
            .unwrap()
            .body
            .contains("http://127.0.0.1:3000/?task="),
        "a cleared address did not fall back: {}",
        sent.last().unwrap().body
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// -- live announcements -----------------------------------------------------

/// Drains whatever the store has announced so far into the set of topic names.
/// `try_recv` rather than `recv` on purpose: the point is what has ALREADY been
/// said by the time the write returned, not what might arrive later.
fn announced(rx: &mut tokio::sync::broadcast::Receiver<izlek_core::Change>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(change) = rx.try_recv() {
        seen.push(change.topic.kind().to_string());
    }
    seen
}

/// Every family of write announces the surface it changed. This is the property
/// the whole live layer rests on: a surface nobody announces is a surface that
/// stays stale until the reader reloads by hand.
#[tokio::test]
async fn every_kind_of_write_announces_its_surface() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let board = store.board(&workspace).await.unwrap().unwrap();
    let column_id = column_named(store, &workspace, "Backlog").await;
    let now = OffsetDateTime::now_utc();

    let mut rx = store.subscribe();

    // Board + Task + Activity.
    store
        .create_task(NewTask {
            clock_at: None,
            board_id: &board.id,
            column_id: &column_id,
            parent_id: None,
            title: "Ship the exporter",
            description: "",
            deadline: None,
            created_by: &admin,
        })
        .await
        .unwrap();
    // Members.
    member(store, &workspace, "bo@izlek.sh", "Bo").await;
    // Queue.
    store
        .queue_notice("bo@izlek.sh", "Subject", "Body", now)
        .await
        .unwrap();
    // Rules.
    store
        .create_mail_rule(
            &board.id,
            &Trigger::StatusBecomes(Some(column_id.clone())),
            "Something moved",
            Audience::Board,
            now,
            false,
        )
        .await
        .unwrap();
    // Settings.
    store
        .set_public_url(&workspace, Some("https://izlek.example"))
        .await
        .unwrap();
    // Activity with no task of its own.
    store
        .record_event(
            Some(&admin),
            &izlek_core::detail::ActivityKind::Other("settings".to_string()),
            "",
            now,
        )
        .await
        .unwrap();

    let seen = announced(&mut rx);
    for topic in [
        "board", "task", "members", "queue", "rules", "settings", "activity",
    ] {
        assert!(
            seen.iter().any(|k| k == topic),
            "no {topic} announcement; heard {seen:?}"
        );
    }
}

/// A write that failed says nothing. A client woken by a change that never
/// committed would re-read the state it already had — harmless once, and a
/// permanent background hum if every refused mail sent one.
#[tokio::test]
async fn a_write_that_failed_announces_nothing() {
    let (scratch, _workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let mut rx = store.subscribe();

    let missing = Ulid::new().to_string();
    assert!(matches!(
        store
            .record_send_accepted(&missing, OffsetDateTime::now_utc())
            .await,
        Err(StoreError::NotFound)
    ));

    assert!(
        announced(&mut rx).is_empty(),
        "a refused write announced a change"
    );
}

/// Every write announces, and this is what keeps it true tomorrow.
///
/// The live layer's whole promise — that no surface goes stale until somebody
/// reloads by hand — rests on one thing: a method that changes a row also says
/// so. That is not a property a running test can observe in general, because
/// the failure is a method nobody wrote a test for. So it is checked against
/// the source instead: every method of the `Store` impl that issues an INSERT,
/// UPDATE or DELETE must also announce, and a method that deliberately does
/// not has to say so here, by name, with a reason.
///
/// A new write with no announcement fails this test rather than shipping a
/// screen that quietly stops updating.
#[test]
fn every_writing_method_announces_or_is_named_here() {
    // Rate-limit bookkeeping. No surface renders an auth attempt, and
    // announcing one would wake every connected client on every failed
    // sign-in — traffic bought for a screen that would look identical.
    const SILENT_ON_PURPOSE: &[&str] = &[
        "record_auth_attempt",
        "clear_auth_attempts",
        "prune_auth_attempts",
        // A lease is one pass saying "mine" for the length of a send, and the
        // write that follows it — accepted, refused, held — announces. Saying
        // so twice would wake every open queue screen on the way past a value
        // that is about to be overwritten.
        "claim_sends_owed",
    ];

    let source = include_str!("../src/store/turso_store.rs");
    let start = source
        .find("impl Store for TursoStore {")
        .expect("the Store impl moved");
    // The impl ends at the first line that is a lone `}` in column zero.
    let end = source[start..]
        .find("\n}\n")
        .map(|at| start + at)
        .expect("unterminated impl block");
    let impl_block = &source[start..end];

    let mut silent = Vec::new();
    let mut methods = impl_block.split("\n    async fn ").skip(1).peekable();
    while let Some(chunk) = methods.next() {
        let name = chunk
            .split(['(', '<', ' '])
            .next()
            .expect("a method with no name");
        let writes = chunk.contains("INSERT INTO")
            || chunk.contains("UPDATE ")
            || chunk.contains("DELETE FROM");
        if writes && !chunk.contains("announce(") && !SILENT_ON_PURPOSE.contains(&name) {
            silent.push(name.to_string());
        }
    }

    assert!(
        silent.is_empty(),
        "these methods change rows without announcing it, so the screens \
         showing those rows will go stale until someone reloads by hand: {silent:?}. \
         Either call `self.announce(..)` after the write commits, or add the \
         method to SILENT_ON_PURPOSE with the reason."
    );
}

/// The sweep sleeps on this answer, so it has to be the earliest moment
/// anything is owed — not merely some moment, and not the newest.
#[tokio::test]
async fn the_next_due_moment_is_the_earliest_one() {
    let (scratch, _workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;

    assert_eq!(
        store.next_due_at().await.unwrap(),
        None,
        "an empty queue owes nothing"
    );

    let now = OffsetDateTime::now_utc();
    // Queued out of order on purpose: the answer is the earliest, not the first.
    store
        .queue_notice("late@izlek.sh", "Later", "body", now + Duration::hours(2))
        .await
        .unwrap();
    store
        .queue_notice(
            "soon@izlek.sh",
            "Sooner",
            "body",
            now + Duration::minutes(5),
        )
        .await
        .unwrap();

    let due = store.next_due_at().await.unwrap().expect("nothing owed");
    // Stored to the second, so compare at that grain.
    assert!(
        (due - (now + Duration::minutes(5))).abs() < Duration::seconds(1),
        "expected the sooner of the two, got {due}"
    );
}

/// A handshake and a delivered mail are different facts, kept in different
/// columns, and editing the sender invalidates both — they were about a server
/// that is no longer the one configured.
#[tokio::test]
async fn a_sender_check_is_recorded_apart_from_a_test_and_cleared_on_edit() {
    let (scratch, workspace, _admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let sender = |host: &str| NewSender {
        host: host.to_string(),
        port: 465,
        username: "izlek".into(),
        password: Some("hunter2".into()),
        from_name: "İzlek".into(),
        from_address: "izlek@izlek.sh".into(),
    };
    store
        .set_sender(&workspace, sender("smtp.one.test"))
        .await
        .unwrap();

    // A login that worked, and a mail that did not: both true at once, and
    // neither may be rendered as the other.
    store
        .record_sender_check(
            &workspace,
            izlek_core::store::SenderCheck {
                at: now,
                took_ms: 120,
                error: None,
            },
        )
        .await
        .unwrap();
    store
        .record_sender_test(
            &workspace,
            izlek_core::store::SenderTest {
                at: now,
                took_ms: 0,
                error: Some("550 not allowed".into()),
            },
        )
        .await
        .unwrap();

    let ws = store.workspace().await.unwrap().unwrap();
    let check = ws.sender_check.expect("no check recorded");
    assert_eq!(check.error, None, "the handshake succeeded");
    assert_eq!(check.took_ms, 120);
    assert_eq!(
        ws.sender_test.expect("no test recorded").error.as_deref(),
        Some("550 not allowed"),
        "the send still failed, and says why"
    );

    // Point it at a different server: what was known is now about nothing.
    store
        .set_sender(&workspace, sender("smtp.two.test"))
        .await
        .unwrap();
    let ws = store.workspace().await.unwrap().unwrap();
    assert!(
        ws.sender_check.is_none(),
        "a stale handshake survived an edit"
    );
    assert!(ws.sender_test.is_none(), "a stale test survived an edit");
}

// ---------------------------------------------------------------------------
// izlek reconcile — bringing a database of the OLD shape onto the declared
// schema. These tests carry the user's rule (2026-08-31): a table alteration
// owes both a proof that the alteration holds and a proof that reconcile
// carries a live-shaped database across it.
// ---------------------------------------------------------------------------

/// Builds a database at the PRE-COLLAPSE schema — the shape İzlek was actually
/// deployed with, kept verbatim in `tests/fixtures/` — and fills it with the
/// kinds of row a live workspace holds, blobs included.
async fn live_shaped_database() -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("izlek-reconcile-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_str().unwrap().to_string();

    let db = turso::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute("PRAGMA foreign_keys = ON", ()).await.unwrap();
    conn.execute_batch(include_str!("fixtures/pre-collapse/0001_init.sql"))
        .await
        .unwrap();
    conn.execute_batch(include_str!("fixtures/pre-collapse/0002_sender_check.sql"))
        .await
        .unwrap();

    let now = "2026-08-30T12:00:00Z";
    let mut sql = String::new();
    sql.push_str(&format!(
        "INSERT INTO workspace (id, name, created_at) VALUES ('W1', 'Dizey', '{now}');"
    ));
    for (id, email, name) in [
        ("U1", "one@example.com", "One"),
        ("U2", "two@example.com", "Two"),
    ] {
        sql.push_str(&format!(
            "INSERT INTO user (id, workspace_id, email, display_name, role, created_at) \
             VALUES ('{id}', 'W1', '{email}', '{name}', 'member', '{now}');"
        ));
    }
    sql.push_str(&format!(
        "INSERT INTO workspace_owner (singleton, user_id, claimed_at) VALUES (1, 'U1', '{now}');"
    ));
    // Two boards, so a task landing on the WRONG board's default tag is
    // visible rather than accidentally correct.
    for b in ["B1", "B2"] {
        sql.push_str(&format!(
            "INSERT INTO board (id, workspace_id, name, created_at) VALUES ('{b}', 'W1', '{b}', '{now}');"
        ));
        sql.push_str(&format!(
            "INSERT INTO board_column (id, board_id, name, position, is_done) \
             VALUES ('C{b}', '{b}', 'Backlog', 0, 0);"
        ));
    }
    for (t, b) in [("T1", "B1"), ("T2", "B1"), ("T3", "B2")] {
        sql.push_str(&format!(
            "INSERT INTO task (id, board_id, task_key, title, column_id, created_by, created_at, updated_at) \
             VALUES ('{t}', '{b}', '{t}', 'a task', 'C{b}', 'U1', '{now}', '{now}');"
        ));
        sql.push_str(&format!(
            "INSERT INTO task_assignee (task_id, user_id) VALUES ('{t}', 'U2');"
        ));
        sql.push_str(&format!(
            "INSERT INTO activity (id, task_id, actor_id, kind, detail, created_at) \
             VALUES ('A{t}', '{t}', 'U1', 'created', '', '{now}');"
        ));
    }
    // A soft-deleted task still belongs to a tag afterwards.
    sql.push_str(&format!(
        "INSERT INTO task (id, board_id, task_key, title, column_id, created_by, created_at, updated_at, deleted_at) \
         VALUES ('T4', 'B2', 'T4', 'gone', 'CB2', 'U1', '{now}', '{now}', '{now}');"
    ));
    conn.execute_batch(&sql).await.unwrap();

    // A blob, because an attachment that survives as NULL would pass a row
    // count and lose the user's file.
    let bytes: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
    conn.execute(
        "INSERT INTO attachment (id, task_id, file_name, mime_type, size_bytes, bytes, uploaded_by, created_at) \
         VALUES ('F1', 'T1', 'a.bin', 'application/octet-stream', ?1, ?2, 'U1', ?3)",
        turso::params![bytes.len() as i64, bytes.clone(), now],
    )
    .await
    .unwrap();

    (dir, path)
}

async fn scalar(path: &str, sql: &str) -> i64 {
    let db = turso::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn.query(sql, ()).await.unwrap();
    rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
}

fn backups_beside(dir: &PathBuf) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".backup-") && !n.ends_with("-wal") && !n.ends_with("-shm"))
        .collect();
    found.sort();
    found
}

#[tokio::test]
async fn reconcile_carries_a_live_shaped_database_onto_the_declared_schema() {
    let (dir, path) = live_shaped_database().await;

    izlek_core::store::reconcile(
        &path,
        izlek_core::store::ReconcileOptions {
            dry_run: false,
            yes: true,
            auto: true,
        },
    )
    .await
    .expect("reconcile refused a database of the shape we actually deployed");

    let db = turso::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();

    // The schema is now the declared one, exactly.
    let after = izlek_core::store::schema::fingerprint(&conn).await.unwrap();
    let declared = izlek_core::store::schema::declared_fingerprint().await.unwrap();
    assert_eq!(after, declared, "the rebuilt schema is not the declared one");

    // Nothing was dropped on the floor.
    assert_eq!(scalar(&path, "SELECT COUNT(*) FROM user").await, 2);
    assert_eq!(scalar(&path, "SELECT COUNT(*) FROM board").await, 2);
    assert_eq!(scalar(&path, "SELECT COUNT(*) FROM task").await, 4);
    assert_eq!(scalar(&path, "SELECT COUNT(*) FROM task_assignee").await, 3);
    assert_eq!(scalar(&path, "SELECT COUNT(*) FROM activity").await, 3);

    // Every task wears its OWN board's default tag — the step no schema diff
    // could have derived.
    assert_eq!(
        scalar(&path, "SELECT COUNT(*) FROM task WHERE tag_id IS NULL").await,
        0,
        "a task came out of the rebuild with no tag"
    );
    assert_eq!(
        scalar(
            &path,
            "SELECT COUNT(*) FROM task t JOIN tag g ON g.id = t.tag_id \
             WHERE g.board_id = t.board_id AND g.is_default = 1",
        )
        .await,
        4,
        "a task was given another board's tag"
    );
    assert_eq!(
        scalar(&path, "SELECT COUNT(*) FROM tag").await,
        2,
        "each board owes exactly one default tag"
    );

    // The blob survived byte for byte.
    assert_eq!(
        scalar(&path, "SELECT length(bytes) FROM attachment WHERE id = 'F1'").await,
        5000
    );
    assert_eq!(
        scalar(
            &path,
            "SELECT COUNT(*) FROM attachment WHERE id = 'F1' AND hex(substr(bytes, 1, 4)) = '00010203'",
        )
        .await,
        1,
        "the attachment's bytes changed in the rebuild"
    );

    // The quiet window is a column the old shape never had: a workspace that
    // crosses over gets the declared default rather than a NULL the app would
    // have to read around.
    assert_eq!(
        scalar(&path, "SELECT mail_batch_minutes FROM workspace WHERE id = 'W1'").await,
        5,
        "the workspace came across without a quiet window"
    );
    // The reminder lead and the clock are columns the pre-collapse shape never
    // had either: the workspace crosses onto the declared default, and tasks
    // that never carried a meeting instant arrive without one.
    assert_eq!(
        scalar(&path, "SELECT reminder_minutes FROM workspace WHERE id = 'W1'").await,
        15,
        "the workspace came across without a reminder lead"
    );
    assert_eq!(
        scalar(&path, "SELECT COUNT(*) FROM task WHERE clock_at IS NOT NULL").await,
        0,
        "a task came out of the rebuild with a clock it never had"
    );

    // And the references all still point at something.
    let mut broken = conn.query("PRAGMA foreign_key_check", ()).await.unwrap();
    assert!(
        broken.next().await.unwrap().is_none(),
        "the rebuilt database has dangling references"
    );

    // The original is beside it, still holding the old shape.
    let backups = backups_beside(&dir);
    assert_eq!(backups.len(), 1, "the rebuild did not keep a backup");
    let backup = dir.join(&backups[0]).to_str().unwrap().to_string();
    assert_eq!(
        scalar(
            &backup,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'tag'",
        )
        .await,
        0,
        "the backup is not the old-shaped database we started from"
    );
    assert_eq!(
        scalar(&backup, "SELECT COUNT(*) FROM task").await,
        4,
        "the backup lost rows"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reconciling_a_database_that_already_matches_changes_nothing() {
    let scratch = Scratch::open().await;
    let path = scratch.dir.join("izlek.db").to_str().unwrap().to_string();

    izlek_core::store::reconcile(
        &path,
        izlek_core::store::ReconcileOptions {
            dry_run: false,
            yes: true,
            auto: true,
        },
    )
    .await
    .expect("reconcile refused a current database");

    assert!(
        backups_beside(&scratch.dir).is_empty(),
        "a database that needed nothing was backed up anyway"
    );
}

#[tokio::test]
async fn opening_a_stale_database_repairs_it() {
    let (dir, path) = live_shaped_database().await;

    // The boot path: no flag, no prompt — the store comes up on a database of
    // the old shape, having repaired it on the way.
    let store = TursoStore::open(&path)
        .await
        .expect("the store would not open a database it was supposed to repair");
    drop(store);

    let db = turso::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(
        izlek_core::store::schema::fingerprint(&conn).await.unwrap(),
        izlek_core::store::schema::declared_fingerprint().await.unwrap(),
    );
    assert_eq!(scalar(&path, "SELECT COUNT(*) FROM task").await, 4);
    assert_eq!(backups_beside(&dir).len(), 1);

    // Opening it again finds it current and leaves it alone.
    let store = TursoStore::open(&path).await.unwrap();
    drop(store);
    assert_eq!(
        backups_beside(&dir).len(),
        1,
        "a second open rebuilt a database that already matched"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_repairs_leave_two_backups() {
    let (dir, path) = live_shaped_database().await;
    let opts = || izlek_core::store::ReconcileOptions {
        dry_run: false,
        yes: true,
        auto: true,
    };

    izlek_core::store::reconcile(&path, opts()).await.unwrap();
    assert_eq!(backups_beside(&dir).len(), 1);

    // Put the database back to the old shape and repair it again: the first
    // backup must still be there. He keeps every one of them.
    let db = turso::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute("DROP TABLE tag_stub_check", ()).await.ok();
    conn.execute("CREATE TABLE stray (id TEXT PRIMARY KEY)", ())
        .await
        .unwrap();
    drop(conn);
    drop(db);

    izlek_core::store::reconcile(&path, opts()).await.unwrap();
    let backups = backups_beside(&dir);
    assert_eq!(
        backups.len(),
        2,
        "the second repair overwrote the first backup: {backups:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
/// The other half of the alteration: a database that ALREADY carries the
/// quiet window keeps the number its admin set, rather than being handed the
/// default a fresh workspace gets.
#[tokio::test]
async fn a_rebuild_keeps_a_window_the_admin_already_set() {
    let scratch = Scratch::open().await;
    let path = scratch.dir.join("izlek.db").to_str().unwrap().to_string();
    let (workspace, _) = claim(&scratch.store).await;
    let limits = scratch.store.workspace().await.unwrap().unwrap();
    scratch
        .store
        .set_limits(
            &workspace,
            limits.attachment_limit_bytes,
            limits.photo_limit_bytes,
            &limits.allowed_file_types,
            17,
            15,
        )
        .await
        .unwrap();

    // Something the declared schema does not know about: enough to make the
    // database stale, so the rebuild actually runs.
    let db = turso::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE stray (id TEXT PRIMARY KEY)", ())
        .await
        .unwrap();
    drop(conn);
    drop(db);

    izlek_core::store::reconcile(
        &path,
        izlek_core::store::ReconcileOptions {
            dry_run: false,
            yes: true,
            auto: true,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        scalar(&path, "SELECT mail_batch_minutes FROM workspace").await,
        17,
        "the rebuild reset a window the admin had chosen"
    );
    assert_eq!(
        scalar(
            &path,
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'stray'",
        )
        .await,
        0,
        "the stray table survived a rebuild onto the declared schema"
    );
}

// --- user stats (the profile page's counts) ---------------------------------

#[tokio::test]
async fn user_stats_counts_what_a_person_holds_finished_opened_and_said() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let store = &scratch.store;
    let mem = member(store, &workspace, "mem@izlek.sh", "Mem Ber").await;
    let backlog = column_named(store, &workspace, "Backlog").await;
    let done = column_named(store, &workspace, "Done").await;
    let now = OffsetDateTime::now_utc();

    let hold = add_task(store, &workspace, "Backlog", "Hold the door", None, &admin).await;
    store.assign_task(&hold, &mem).await.unwrap();
    let finished = add_task(store, &workspace, "Backlog", "Paint the frame", None, &admin).await;
    store.assign_task(&finished, &mem).await.unwrap();
    store.move_task(&finished, &backlog, &done, &admin, now).await.unwrap();
    // One of her own that she also holds: created and assigned must each
    // count it in its own column, not argue about it.
    let own = add_task(store, &workspace, "Backlog", "Sweep", None, &mem).await;
    store.assign_task(&own, &mem).await.unwrap();
    add_task(store, &workspace, "Backlog", "Mop", None, &mem).await;
    add_task(store, &workspace, "Backlog", "Dust", None, &mem).await;
    for i in 0..4 {
        store.add_comment(&hold, &mem, "a note", now + Duration::seconds(i)).await.unwrap();
    }

    assert_eq!(
        store.user_stats(&mem).await.unwrap(),
        izlek_core::store::UserStats { assigned_open: 2, assigned_done: 1, created: 3, comments: 4 },
    );
}

#[tokio::test]
async fn a_deleted_task_counts_nowhere_and_a_person_with_nothing_counts_nothing() {
    let scratch = Scratch::open().await;
    let (workspace, admin) = claim(&scratch.store).await;
    let store = &scratch.store;
    let mem = member(store, &workspace, "mem@izlek.sh", "Mem Ber").await;
    let shy = member(store, &workspace, "shy@izlek.sh", "Shy Guy").await;
    let now = OffsetDateTime::now_utc();

    assert_eq!(
        store.user_stats(&shy).await.unwrap(),
        izlek_core::store::UserStats { assigned_open: 0, assigned_done: 0, created: 0, comments: 0 },
    );

    let doomed = add_task(store, &workspace, "Backlog", "Doomed", None, &mem).await;
    store.assign_task(&doomed, &mem).await.unwrap();
    store.add_comment(&doomed, &mem, "a note", now).await.unwrap();
    assert_eq!(store.user_stats(&mem).await.unwrap().assigned_open, 1);
    assert_eq!(store.user_stats(&mem).await.unwrap().comments, 1);

    store.delete_task(&doomed, &admin, now).await.unwrap();
    assert_eq!(
        store.user_stats(&mem).await.unwrap(),
        izlek_core::store::UserStats { assigned_open: 0, assigned_done: 0, created: 0, comments: 0 },
        "a soft-deleted task stays out of every number, its comments included",
    );
}
// -- the quiet window ------------------------------------------------------

/// Every rule on the board, so one workflow trips several of them.
async fn rules_for_a_workflow(store: &TursoStore, workspace: &str) {
    let board = store.board(workspace).await.unwrap().unwrap();
    let now = OffsetDateTime::now_utc();
    for (trigger, subject, audience) in [
        (Trigger::Created, "New task", Audience::Board),
        (Trigger::Assigned, "You were assigned", Audience::Assignees),
        (Trigger::DeadlineSet, "Deadline set", Audience::Board),
    ] {
        store
            .create_mail_rule(&board.id, &trigger, subject, audience, now, false)
            .await
            .unwrap();
    }
}

/// Hands every activity row a save wrote to the engine, in order.
async fn tell_the_engine(store: &TursoStore, engine: &Engine, ids: &[String]) -> Report {
    let mut total = Report::default();
    for id in ids {
        let event = activity_event(store, id).await;
        let one = engine.on_activity(&event).await.unwrap();
        total.sent += one.sent;
        total.batched += one.batched;
        total.already_owned += one.already_owned;
    }
    total
}

/// His scenario: create a card, assign it, give it a deadline. Three rules,
/// three triggers, one uninterrupted minute of work — and the person who has
/// to read it gets one mail saying where the card ended up.
#[tokio::test]
async fn a_whole_workflow_inside_the_window_is_one_mail() {
    use time::macros::date;
    let (dir, store, workspace, admin) = waiting(5).await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    rules_for_a_workflow(&store, &workspace).await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let board = store.board(&workspace).await.unwrap().unwrap();
    let column_id = column_named(&store, &workspace, "Backlog").await;
    let created = store
        .create_task(NewTask {
            clock_at: None,
            board_id: &board.id,
            column_id: &column_id,
            parent_id: None,
            title: "Ship the exporter",
            description: "",
            deadline: None,
            created_by: &admin,
        })
        .await
        .unwrap();
    let task = created.row.id.clone();
    let first = tell_the_engine(&store, &engine, &[created.activity_id]).await;

    store.assign_task(&task, &mate).await.unwrap();
    let assigned = store
        .record_activity(
            &task,
            Some(&admin),
            Some(&mate),
            &ActivityKind::Assigned,
            "Emre",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let second = tell_the_engine(&store, &engine, &[assigned]).await;

    let ids = store
        .save_task(
            &task,
            "Ship the exporter",
            "",
            Some(date!(2026 - 09 - 30)),
            None,
            &admin,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let third = tell_the_engine(&store, &engine, &ids).await;

    assert_eq!(
        (first.sent, second.sent, third.sent),
        (0, 0, 0),
        "nothing leaves the building while the window is open",
    );
    assert_eq!(
        first.batched + second.batched + third.batched,
        3,
        "three triggers, three rows waiting on one another",
    );
    assert!(mailer.sent().is_empty());

    // The window closes.
    let report = engine
        .deliver_owed(OffsetDateTime::now_utc() + Duration::minutes(6), 10)
        .await
        .unwrap();
    assert_eq!(report.sent, 1, "one envelope for the whole workflow");
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "emre@izlek.sh");
    assert_eq!(
        sent[0].subject, "Deadline set",
        "the newest thing that happened names the mail",
    );
    assert!(
        sent[0].body.contains("Column: Backlog"),
        "the mail states where the card is now: {}",
        sent[0].body,
    );
    assert!(
        sent[0].body.contains("Deadline: 2026-09-30"),
        "and the deadline it ended up with: {}",
        sent[0].body,
    );
    assert!(
        sent[0].body.contains("Assignees: Emre"),
        "and who is on it: {}",
        sent[0].body,
    );

    // Nothing is left owed, and a second pass has nothing to send.
    let again = engine
        .deliver_owed(OffsetDateTime::now_utc() + Duration::hours(2), 10)
        .await
        .unwrap();
    assert_eq!(again.sent, 0);
    assert_eq!(mailer.sent().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The mistake case: a card goes into the wrong column and is put right a
/// moment later. One mail, naming the column it is actually in.
#[tokio::test]
async fn a_column_put_right_inside_the_window_mails_only_where_the_card_ended_up() {
    let (dir, store, workspace, admin) = waiting(5).await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    a_rule(&store, &workspace, "Done", "Card is done").await;
    a_rule(&store, &workspace, "In Progress", "Card is in progress").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let wrong = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    assert_eq!(engine.on_transition(&wrong).await.unwrap().sent, 0);
    let right = moved_to(&store, &workspace, &task, "Done", "In Progress", &admin).await;
    assert_eq!(engine.on_transition(&right).await.unwrap().sent, 0);

    let report = engine
        .deliver_owed(OffsetDateTime::now_utc() + Duration::minutes(6), 10)
        .await
        .unwrap();
    assert_eq!(report.sent, 1);
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].subject, "Card is in progress");
    assert!(
        sent[0].body.contains("Column: In Progress"),
        "the mail says where the card is, not where it was: {}",
        sent[0].body,
    );
    assert!(
        !sent[0].body.contains("Column: Done"),
        "the mistake is not mailed to anybody: {}",
        sent[0].body,
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A second event about the same card pushes the first one's mail out again:
/// the window is quiet time, not a fixed timer from the first trigger.
#[tokio::test]
async fn a_second_event_pushes_the_batch_out_again() {
    let (dir, store, workspace, admin) = waiting(5).await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    a_rule(&store, &workspace, "Done", "Card is done").await;
    a_rule(&store, &workspace, "In Progress", "Card is in progress").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let first = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    engine.on_transition(&first).await.unwrap();
    let due_then = the_one_row(&store).await.next_attempt_at.unwrap();

    let second = moved_to(&store, &workspace, &task, "Done", "In Progress", &admin).await;
    engine.on_transition(&second).await.unwrap();
    let rows = store
        .sends_owed(OffsetDateTime::now_utc() + Duration::hours(2), 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "two triggers, two rows");
    for row in &rows {
        assert!(
            row.next_attempt_at.unwrap() > due_then,
            "the first row waits for the second one's window too",
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reads the single pending row back.
async fn the_one_row(store: &TursoStore) -> izlek_core::store::MailSend {
    let mut rows = store
        .sends_owed(OffsetDateTime::now_utc() + Duration::hours(2), 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one owed row");
    rows.remove(0)
}

/// A card somebody edits all afternoon would never settle, so the push has a
/// ceiling measured from the oldest mail in the batch: after four windows it
/// goes out with whatever it has.
#[tokio::test]
async fn the_hold_has_a_ceiling_measured_from_the_oldest_mail() {
    let (dir, store, workspace, admin) = waiting(1).await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Card is done").await;

    // A row born ten minutes ago: the afternoon-long edit, without the
    // afternoon.
    let born = OffsetDateTime::now_utc() - Duration::minutes(10);
    store
        .claim_send(&rule.id, "event-1", &task, "emre@izlek.sh", born, born)
        .await
        .unwrap()
        .expect("a fresh row is claimed");

    let far = OffsetDateTime::now_utc() + Duration::minutes(5);
    store
        .hold_batch(&task, "emre@izlek.sh", far, Duration::minutes(4))
        .await
        .unwrap();

    let row = the_one_row(&store).await;
    let due = row.next_attempt_at.unwrap();
    assert!(
        due < far,
        "the push is clipped rather than granted: {due} vs {far}",
    );
    assert_eq!(
        due, born + Duration::minutes(4),
        "the ceiling is the oldest mail's own patience running out",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hold only ever postpones. The delivery pass writes its lease into the
/// same column, so a hold that pulled a leased row back would make the mail
/// one pass is composing due for another — and the reader would get it twice.
#[tokio::test]
async fn a_hold_never_pulls_a_leased_row_back_under_the_pass_that_took_it() {
    let (dir, store, workspace, admin) = waiting(1).await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rule = a_rule(&store, &workspace, "Done", "Card is done").await;

    let now = OffsetDateTime::now_utc();
    store
        .claim_send(&rule.id, "event-1", &task, "emre@izlek.sh", now, now)
        .await
        .unwrap()
        .expect("a fresh row is claimed");
    // A delivery pass takes it: the lease is five minutes out.
    let taken = store
        .claim_sends_owed(now + Duration::seconds(1), now + izlek_core::mail::LEASE, 10)
        .await
        .unwrap();
    assert_eq!(taken.len(), 1, "the pass did not take the row");
    let leased = the_one_row(&store).await.next_attempt_at.unwrap();

    // A new trigger on the same card, one quiet minute out: earlier than the
    // lease, so it must change nothing.
    store
        .hold_batch(
            &task,
            "emre@izlek.sh",
            now + Duration::minutes(1),
            Duration::minutes(4),
        )
        .await
        .unwrap();
    assert_eq!(
        the_one_row(&store).await.next_attempt_at.unwrap(),
        leased,
        "the hold pulled a row out from under the pass holding it"
    );

    // A hold past the lease is a postponement, and does apply.
    store
        .hold_batch(
            &task,
            "emre@izlek.sh",
            now + Duration::minutes(30),
            Duration::hours(2),
        )
        .await
        .unwrap();
    assert!(
        the_one_row(&store).await.next_attempt_at.unwrap() > leased,
        "a real postponement was refused too"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A workspace with no window is İzlek as it was: a trigger is a mail, sent
/// the moment it is owed.
#[tokio::test]
async fn no_window_sends_each_trigger_as_it_happens() {
    let (dir, store, workspace, admin) = waiting(0).await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    a_rule(&store, &workspace, "Done", "Card is done").await;
    a_rule(&store, &workspace, "In Progress", "Card is in progress").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let first = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    assert_eq!(engine.on_transition(&first).await.unwrap().sent, 1);
    let second = moved_to(&store, &workspace, &task, "Done", "In Progress", &admin).await;
    assert_eq!(engine.on_transition(&second).await.unwrap().sent, 1);
    assert_eq!(mailer.sent().len(), 2, "two crossings, two mails");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The batch is one card and one reader. Two cards are two mails, and two
/// people are two mails — nobody is told about somebody else's card because
/// the clock happened to line up.
#[tokio::test]
async fn a_batch_is_one_card_and_one_reader() {
    let (dir, store, workspace, admin) = waiting(5).await;
    let emre = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let grace = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let first = add_task(&store, &workspace, "Backlog", "First", None, &admin).await;
    let second = add_task(&store, &workspace, "Backlog", "Second", None, &admin).await;
    for task in [&first, &second] {
        store.assign_task(task, &emre).await.unwrap();
        store.assign_task(task, &grace).await.unwrap();
    }
    a_rule(&store, &workspace, "Done", "Card is done").await;

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    for task in [&first, &second] {
        let crossing = moved_to(&store, &workspace, task, "Backlog", "Done", &admin).await;
        engine.on_transition(&crossing).await.unwrap();
    }

    let report = engine
        .deliver_owed(OffsetDateTime::now_utc() + Duration::minutes(6), 10)
        .await
        .unwrap();
    assert_eq!(report.sent, 4, "two cards times two readers");
    let mut addressed: Vec<String> = mailer
        .sent()
        .into_iter()
        .map(|mail| format!("{} {}", mail.to, mail.body.lines().next().unwrap_or_default()))
        .collect();
    addressed.sort();
    assert_eq!(addressed.len(), 4);
    assert_eq!(
        addressed.iter().filter(|line| line.contains("First")).count(),
        2,
    );
    assert_eq!(
        addressed.iter().filter(|line| line.contains("emre@izlek.sh")).count(),
        2,
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An invitation belongs to nobody's card: it carries its own subject and body
/// and is never folded into a batch, however long the window is.
#[tokio::test]
async fn an_invitation_is_never_folded_into_a_batch() {
    let (dir, store, workspace, admin) = waiting(5).await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    a_rule(&store, &workspace, "Done", "Card is done").await;
    store
        .queue_invite(
            "emre@izlek.sh",
            "Join the workspace",
            "Your link: https://izlek.sh/join",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let crossing = moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    engine.on_transition(&crossing).await.unwrap();

    let report = engine
        .deliver_owed(OffsetDateTime::now_utc() + Duration::minutes(6), 10)
        .await
        .unwrap();
    assert_eq!(report.sent, 2, "the invitation and the card's mail");
    let subjects: Vec<String> = mailer
        .sent()
        .into_iter()
        .map(|mail| mail.subject)
        .collect();
    assert!(subjects.contains(&"Join the workspace".to_string()), "{subjects:?}");
    assert!(subjects.contains(&"Card is done".to_string()), "{subjects:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A batch that the server refuses is one refusal for every row in it, and
/// the retry is the same batch again — not one mail per row once the ledger
/// has been touched.
#[tokio::test]
async fn a_refused_batch_is_retried_as_a_batch() {
    let (dir, store, workspace, admin) = waiting(5).await;
    let mate = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = add_task(&store, &workspace, "Backlog", "Ship it", None, &admin).await;
    store.assign_task(&task, &mate).await.unwrap();
    a_rule(&store, &workspace, "Done", "Card is done").await;
    a_rule(&store, &workspace, "In Progress", "Card is in progress").await;

    let mailer = Remembering::refusing(vec![MailError::retryable("host is down")]);
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    for (from, to) in [("Backlog", "Done"), ("Done", "In Progress")] {
        let crossing = moved_to(&store, &workspace, &task, from, to, &admin).await;
        engine.on_transition(&crossing).await.unwrap();
    }

    let refused = engine
        .deliver_owed(OffsetDateTime::now_utc() + Duration::minutes(6), 10)
        .await
        .unwrap();
    assert_eq!(refused.failed, 1, "one envelope, one refusal");
    assert!(mailer.sent().is_empty());
    let owed = store
        .sends_owed(OffsetDateTime::now_utc() + Duration::hours(2), 10)
        .await
        .unwrap();
    assert_eq!(owed.len(), 2, "both rows are still owed");
    for row in &owed {
        assert_eq!(row.state, SendState::Failed);
        assert_eq!(row.attempts, 1, "the batch spent one attempt, not two");
    }

    let sent = engine
        .deliver_owed(
            OffsetDateTime::now_utc() + Duration::minutes(6) + backoff(1),
            10,
        )
        .await
        .unwrap();
    assert_eq!(sent.sent, 1, "the retry is one envelope as well");
    assert_eq!(mailer.sent().len(), 1);
    assert!(
        store
            .sends_owed(OffsetDateTime::now_utc() + Duration::hours(2), 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing is left owed",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The task clock and its reminders: a task can carry an exact meeting instant,
// and the queue owes every assignee one reminder mail, reminder_minutes before
// the clock. Every task write that can move the grounds re-derives what is
// owed, inside the same transaction as the write itself.
// ---------------------------------------------------------------------------

use izlek_core::store::MailSend;

/// A task carrying a meeting instant `clock` from now (or none), as the
/// reminder tests keep needing.
async fn clocked_task(
    store: &TursoStore,
    workspace_id: &str,
    author: &str,
    clock: Option<OffsetDateTime>,
) -> String {
    let board = store.board(workspace_id).await.unwrap().unwrap();
    let column_id = column_named(store, workspace_id, "Backlog").await;
    store
        .create_task(NewTask {
            board_id: &board.id,
            column_id: &column_id,
            parent_id: None,
            title: "The quarterly review",
            description: "",
            deadline: None,
            clock_at: clock,
            created_by: author,
        })
        .await
        .unwrap()
        .row
        .id
}

/// Every reminder row the task ever owed, one per row, recipient-ordered.
/// `sends_for_task` reads the ledger whatever the rows' state, which is the
/// point: abandonment is as much a fact as a send.
async fn reminders(store: &TursoStore, task: &str) -> Vec<MailSend> {
    let mut rows: Vec<MailSend> = store
        .sends_for_task(task, 50)
        .await
        .unwrap()
        .into_iter()
        .filter(|send| send.kind == SendKind::Reminder)
        .collect();
    rows.sort_by(|a, b| a.recipient.cmp(&b.recipient));
    rows
}

/// Just the reminders still owed: the re-derive leaves abandoned history
/// behind it, and what the queue will deliver is the pending set.
async fn pending_reminders(store: &TursoStore, task: &str) -> Vec<MailSend> {
    let mut rows: Vec<MailSend> = reminders(store, task)
        .await
        .into_iter()
        .filter(|send| send.state == SendState::Pending)
        .collect();
    rows.sort_by(|a, b| a.recipient.cmp(&b.recipient));
    rows
}

async fn set_reminder_lead(store: &TursoStore, workspace_id: &str, minutes: u32) {
    let limits = store.workspace().await.unwrap().unwrap();
    store
        .set_limits(
            workspace_id,
            limits.attachment_limit_bytes,
            limits.photo_limit_bytes,
            &limits.allowed_file_types,
            limits.mail_batch_minutes,
            minutes,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_clocked_task_owes_each_assignee_one_reminder_before_the_meeting() {
    let (dir, store, workspace, admin) = shared().await;
    let grace = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let emre = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let now = OffsetDateTime::now_utc();
    let clock = now + Duration::hours(2);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &grace).await.unwrap();
    store.assign_task(&task, &emre).await.unwrap();

    let rows = pending_reminders(&store, &task).await;
    assert_eq!(rows.len(), 2, "one reminder per assignee: {rows:?}");
    for row in &rows {
        assert_eq!(row.state, SendState::Pending);
        assert_eq!(
            row.next_attempt_at.unwrap(),
            clock - Duration::minutes(15),
            "the workspace's default lead of fifteen minutes"
        );
        assert!(
            row.subject.as_deref().unwrap().starts_with("Reminder: The quarterly review ("),
            "the subject names the meeting: {}",
            row.subject.as_deref().unwrap()
        );
        let body = row.body.as_deref().unwrap();
        assert!(body.contains("The quarterly review"), "body was: {body}");
        assert!(body.contains("Meets at"), "body was: {body}");
        // The countdown is whole minutes at mint time, and a few ticks pass
        // between picking the clock and creating the task.
        let minutes: u32 = body
            .split("in ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        assert!(
            (117..=120).contains(&minutes),
            "the countdown names the meeting two hours out: {body}"
        );
    }
    assert_eq!(rows[0].recipient, "emre@izlek.sh");
    assert_eq!(rows[1].recipient, "grace@izlek.sh");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reminders_switched_off_owe_nobody() {
    let (dir, store, workspace, admin) = shared().await;
    set_reminder_lead(&store, &workspace, 0).await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let clock = OffsetDateTime::now_utc() + Duration::hours(2);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &mate).await.unwrap();
    assert!(
        reminders(&store, &task).await.is_empty(),
        "a lead of zero is the off switch"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_reminder_falls_due_at_the_workspaces_own_lead() {
    let (dir, store, workspace, admin) = shared().await;
    set_reminder_lead(&store, &workspace, 60).await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let clock = OffsetDateTime::now_utc() + Duration::hours(4);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &mate).await.unwrap();
    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].next_attempt_at.unwrap(),
        clock - Duration::minutes(60),
        "an hour before the meeting, not the default quarter"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn moving_the_clock_re_makes_the_reminders() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let task = clocked_task(
        &store,
        &workspace,
        &admin,
        Some(OffsetDateTime::now_utc() + Duration::hours(2)),
    )
    .await;
    store.assign_task(&task, &mate).await.unwrap();
    let first = reminders(&store, &task).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].state, SendState::Pending);

    let new_clock = OffsetDateTime::now_utc() + Duration::hours(10);
    store
        .save_task(
            &task,
            "The quarterly review",
            "",
            None,
            Some(new_clock),
            &admin,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 2, "the old promise and the new one: {rows:?}");
    let pending: Vec<_> = rows
        .iter()
        .filter(|r| r.state == SendState::Pending)
        .collect();
    let abandoned: Vec<_> = rows
        .iter()
        .filter(|r| r.state == SendState::Abandoned)
        .collect();
    assert_eq!(pending.len(), 1, "exactly one reminder is still owed");
    assert_eq!(abandoned.len(), 1, "the promise the old clock stood on died");
    assert_eq!(
        pending[0].next_attempt_at.unwrap(),
        new_clock - Duration::minutes(15),
        "the new reminder follows the new clock"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn clearing_the_clock_abandons_the_reminders() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let task = clocked_task(
        &store,
        &workspace,
        &admin,
        Some(OffsetDateTime::now_utc() + Duration::hours(2)),
    )
    .await;
    store.assign_task(&task, &mate).await.unwrap();

    store
        .save_task(
            &task,
            "The quarterly review",
            "",
            None,
            None,
            &admin,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].state,
        SendState::Abandoned,
        "no meeting, no warning"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_assignee_added_after_the_clock_gets_a_reminder() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let task = clocked_task(
        &store,
        &workspace,
        &admin,
        Some(OffsetDateTime::now_utc() + Duration::hours(2)),
    )
    .await;
    assert!(reminders(&store, &task).await.is_empty());

    store.assign_task(&task, &mate).await.unwrap();
    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 1, "the person who joined is warned too");
    assert_eq!(rows[0].state, SendState::Pending);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_assignee_removed_takes_their_reminder_with_them() {
    let (dir, store, workspace, admin) = shared().await;
    let grace = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let emre = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let task = clocked_task(
        &store,
        &workspace,
        &admin,
        Some(OffsetDateTime::now_utc() + Duration::hours(2)),
    )
    .await;
    store.assign_task(&task, &grace).await.unwrap();
    store.assign_task(&task, &emre).await.unwrap();

    store.unassign_task(&task, &grace).await.unwrap();
    // Grace's warning is gone from what is owed; Emre's stands.
    let rows = reminders(&store, &task).await;
    assert!(
        rows.iter().any(|r| r.recipient == "grace@izlek.sh"
            && r.state == SendState::Abandoned),
        "the removed person's reminder was abandoned: {rows:?}"
    );
    let still_owed = pending_reminders(&store, &task).await;
    assert_eq!(still_owed.len(), 1);
    assert_eq!(still_owed[0].recipient, "emre@izlek.sh");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn finishing_a_task_abandons_its_reminders() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let task = clocked_task(
        &store,
        &workspace,
        &admin,
        Some(OffsetDateTime::now_utc() + Duration::hours(2)),
    )
    .await;
    store.assign_task(&task, &mate).await.unwrap();

    moved_to(&store, &workspace, &task, "Backlog", "Done", &admin).await;
    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].state,
        SendState::Abandoned,
        "a finished meeting warns nobody"
    );

    // Reopening re-derives: the meeting has not happened, so the warning is
    // owed again.
    moved_to(&store, &workspace, &task, "Done", "Backlog", &admin).await;
    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().any(|r| r.state == SendState::Pending),
        "a reopened task owes its people a warning again"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn deleting_a_task_abandons_its_reminders() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let task = clocked_task(
        &store,
        &workspace,
        &admin,
        Some(OffsetDateTime::now_utc() + Duration::hours(2)),
    )
    .await;
    store.assign_task(&task, &mate).await.unwrap();

    store
        .delete_task(&task, &admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, SendState::Abandoned);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_clock_in_the_past_owes_nobody_a_warning() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let clock = OffsetDateTime::now_utc() - Duration::hours(1);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &mate).await.unwrap();
    assert!(
        reminders(&store, &task).await.is_empty(),
        "a meeting that already happened is not warned about"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_clocked_task_without_assignees_owes_nobody() {
    let (dir, store, workspace, admin) = shared().await;
    let clock = OffsetDateTime::now_utc() + Duration::hours(2);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    assert!(reminders(&store, &task).await.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_meeting_already_inside_the_window_warns_the_moment_it_is_owed() {
    let (dir, store, workspace, admin) = shared().await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    // Five minutes out, against a fifteen-minute lead: the due instant is
    // already past, so the row is born due now rather than in the past.
    let now = OffsetDateTime::now_utc();
    let clock = now + Duration::minutes(5);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &mate).await.unwrap();

    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 1);
    let due = rows[0].next_attempt_at.unwrap();
    assert!(
        due >= now && due <= OffsetDateTime::now_utc() + Duration::seconds(5),
        "the reminder is due immediately, not before now: {due}"
    );
    let body = rows[0].body.as_deref().unwrap();
    // Whole minutes at mint time, and the clock was picked a moment before
    // the write: anything from four up is the same five-minute meeting.
    assert!(
        body.contains("in 4 minutes") || body.contains("in 5 minutes"),
        "body was: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn each_reminder_tells_the_meeting_in_its_own_recipients_clock() {
    let (dir, store, workspace, admin) = shared().await;
    let grace = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let emre = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    // Emre reads the world three hours ahead of the stored UTC.
    store
        .set_preferences(&emre, "UTC+03:00", "dark", "en", "default")
        .await
        .unwrap();
    let clock = OffsetDateTime::parse("2026-09-15T12:00:00Z", &time::format_description::well_known::Rfc3339)
        .unwrap();
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &grace).await.unwrap();
    store.assign_task(&task, &emre).await.unwrap();

    let rows = pending_reminders(&store, &task).await;
    assert_eq!(rows.len(), 2);
    let grace_body = rows.iter().find(|r| r.recipient == "grace@izlek.sh").unwrap().body.as_deref().unwrap();
    let emre_body = rows.iter().find(|r| r.recipient == "emre@izlek.sh").unwrap().body.as_deref().unwrap();
    assert!(grace_body.contains("Meets at Sep 15 12:00"), "grace: {grace_body}");
    assert!(emre_body.contains("Meets at Sep 15 15:00"), "emre: {emre_body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reminder_rows_survive_a_reopen() {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();
    let store = TursoStore::open(&path).await.unwrap();
    let (workspace, admin) = claim(&store).await;
    let mate = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let clock = OffsetDateTime::now_utc() + Duration::hours(2);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &mate).await.unwrap();
    let before = reminders(&store, &task).await;
    assert_eq!(before.len(), 1);
    let before = before;
    drop(store);

    let reopened = TursoStore::open(&path).await.unwrap();
    let after = reminders(&reopened, &task).await;
    assert_eq!(after.len(), before.len());
    assert_eq!(after[0].id, before[0].id, "the same row, not a re-minted one");
    assert_eq!(after[0].state, SendState::Pending);
    assert_eq!(after[0].next_attempt_at, before[0].next_attempt_at);
    let _ = std::fs::remove_dir_all(&dir);
}
#[tokio::test]
async fn a_reminder_already_sent_is_never_queued_again() {
    let (dir, store, workspace, admin) = shared().await;
    let grace = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let emre = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    let clock = OffsetDateTime::now_utc() + Duration::hours(2);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &grace).await.unwrap();
    store.assign_task(&task, &emre).await.unwrap();

    let mailer = Remembering::taking_everything();
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    // The meeting is 15 minutes out from due, so a pass anywhere after that
    // delivers both reminders.
    let later = clock - Duration::minutes(14);
    assert_eq!(engine.deliver_owed(later, 10).await.unwrap().sent, 2);

    // Any later write about the card re-derives the reminders. The two who
    // were reminded keep the rows that went out; nobody is minted a second.
    let yusuf = member(&store, &workspace, "yusuf@izlek.sh", "Yusuf").await;
    store.assign_task(&task, &yusuf).await.unwrap();
    let rows = reminders(&store, &task).await;
    let yusuf_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.recipient == "yusuf@izlek.sh")
        .collect();
    assert_eq!(yusuf_rows.len(), 1, "the newcomer has one row: {rows:?}");
    let served: Vec<_> = rows
        .iter()
        .filter(|r| r.recipient != "yusuf@izlek.sh")
        .collect();
    assert!(
        served.iter().all(|r| r.state != SendState::Pending),
        "nobody served is owed again: {served:?}"
    );
    for person in ["grace@izlek.sh", "emre@izlek.sh"] {
        assert_eq!(
            served
                .iter()
                .filter(|r| r.recipient == person && r.state == SendState::Sent)
                .count(),
            1,
            "{person} was delivered exactly once: {served:?}"
        );
    }
    let pending = pending_reminders(&store, &task).await;
    assert_eq!(pending.len(), 1, "only the newcomer is owed: {pending:?}");
    assert_eq!(pending[0].recipient, "yusuf@izlek.sh");

    // And the queue itself repeats nothing: sweep after sweep re-delivers
    // nobody, whatever the writes in between did. Taking Yusuf off the card
    // and putting him back is a write pair every round — the sync his
    // reminder rides abandons and re-derives each time.
    for _ in 0..3 {
        engine
            .deliver_owed(later + Duration::minutes(1), 10)
            .await
            .unwrap();
        store.unassign_task(&task, &yusuf).await.unwrap();
        store.assign_task(&task, &yusuf).await.unwrap();
    }
    assert_eq!(mailer.sent().len(), 3, "{:?}", mailer.sent());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_reminder_failing_on_retries_is_not_minted_twice() {
    let (dir, store, workspace, admin) = shared().await;
    let grace = member(&store, &workspace, "grace@izlek.sh", "Grace").await;
    let clock = OffsetDateTime::now_utc() + Duration::hours(2);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store.assign_task(&task, &grace).await.unwrap();

    let mailer = Remembering::refusing(vec![MailError::retryable("host is down")]);
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let later = clock - Duration::minutes(14);
    assert_eq!(engine.deliver_owed(later, 10).await.unwrap().failed, 1);

    // The write between the sweeps finds Grace's reminder riding its own
    // retry clock, and leaves it exactly one row.
    let emre = member(&store, &workspace, "emre@izlek.sh", "Emre").await;
    store.assign_task(&task, &emre).await.unwrap();
    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 2, "one row per person: {rows:?}");
    let grace_row = rows.iter().find(|r| r.recipient == "grace@izlek.sh").unwrap();
    assert_eq!(grace_row.state, SendState::Failed);
    assert_eq!(grace_row.attempts, 1);

    // When the host comes back the retry lands once — not once per row.
    engine
        .deliver_owed(later + backoff(1) + Duration::minutes(1), 10)
        .await
        .unwrap();
    let grace_mail = mailer
        .sent()
        .iter()
        .filter(|m| m.to == "grace@izlek.sh")
        .count();
    assert_eq!(grace_mail, 1, "{:?}", mailer.sent());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_reminder_never_lands_at_or_after_the_deadline() {
    let (dir, store, workspace, admin) = shared().await;
    // A meeting comfortably ahead: the lead decides the moment.
    let clock = OffsetDateTime::now_utc() + Duration::hours(2);
    let task = clocked_task(&store, &workspace, &admin, Some(clock)).await;
    store
        .assign_task(&task, &member(&store, &workspace, "grace@izlek.sh", "Grace").await)
        .await
        .unwrap();
    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 1);
    let due = rows[0].next_attempt_at.unwrap();
    assert!(
        due < clock,
        "the reminder fires {:#?} before the clock",
        clock - due
    );
    assert_eq!(due, clock - Duration::minutes(15));

    // A meeting already inside the window: the warning commits now, which is
    // still before the clock.
    let soon = OffsetDateTime::now_utc() + Duration::minutes(10);
    let task = clocked_task(&store, &workspace, &admin, Some(soon)).await;
    store
        .assign_task(&task, &member(&store, &workspace, "emre@izlek.sh", "Emre").await)
        .await
        .unwrap();
    let rows = reminders(&store, &task).await;
    assert_eq!(rows.len(), 1);
    let due = rows[0].next_attempt_at.unwrap();
    assert!(due < soon, "due {due} at or after the clock {soon}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Setting the password back to what it already is is refused by name, and a
/// refused change leaves every session — and the old password — standing.
#[tokio::test]
async fn a_change_back_to_the_same_password_is_refused_and_signs_nobody_out() {
    let (_scratch, accounts, admin) = claimed().await;
    // A second browser holds a session the refused change must not touch.
    let other = accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.8")
        .await
        .unwrap();
    assert!(matches!(
        accounts
            .change_password(
                &admin.id,
                "tide-tables-1892",
                "tide-tables-1892",
                "198.51.100.7"
            )
            .await,
        Err(AccountError::Password(PasswordProblem::IsCurrent))
    ));
    // The other browser is still signed in, and the old password still works.
    assert!(
        accounts
            .authenticate(other.session_token.expose())
            .await
            .unwrap()
            .is_some()
    );
    accounts
        .sign_in("ada@izlek.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
}
