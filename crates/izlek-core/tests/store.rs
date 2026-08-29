//! Integration tests for izlek-core: the storage boundary driven through the
//! Turso implementation, and the account flows on top of it.
//!
//! New integration tests belong in this file rather than a new `tests/*.rs`:
//! one test binary links and runs once.

use std::path::PathBuf;

use izlek_core::Role;
use izlek_core::auth::{Token, hash_password};
use izlek_core::store::{
    Audience, Event, MailOutcome, MailRule, NewAttachment, NewSender, NewUser, SendKind,
    SendState, Store, StoreError, Trigger, TursoStore, User,
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
            "Izlek",
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
            "Izlek",
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

/// Builds a database file shaped like schema version 12 — every migration
/// through 0012 applied, nothing after — by replaying the migration files
/// straight off disk on a raw connection, the same way [`TursoStore::apply`]
/// does it one at a time. Used only to put a rule, a send and a decision in
/// place before 0013 exists, so 0013 runs against real rows rather than an
/// empty table.
async fn a_pre_0013_store_with_a_rule_send_and_decision()
-> (PathBuf, String, String, String, String) {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();

    let db = turso::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute("PRAGMA foreign_keys = ON", ()).await.unwrap();
    conn.execute(
        "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        (),
    )
    .await
    .unwrap();

    let migrations_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<_> = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n < "0013")
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    for (i, file) in files.iter().enumerate() {
        let sql = std::fs::read_to_string(file).unwrap();
        conn.execute_batch(&sql).await.unwrap();
        conn.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (?1, '2026-08-26T00:00:00Z')",
            turso::params![(i + 1) as i64],
        )
        .await
        .unwrap();
    }

    let workspace = Ulid::new().to_string();
    let admin = Ulid::new().to_string();
    let board = Ulid::new().to_string();
    let backlog = Ulid::new().to_string();
    let done = Ulid::new().to_string();
    let task = Ulid::new().to_string();
    let rule = Ulid::new().to_string();
    let transition = Ulid::new().to_string();
    conn.execute(
        "INSERT INTO workspace (id, name, created_at) VALUES (?1, 'Izlek', '2026-08-26T00:00:00Z')",
        turso::params![workspace.clone()],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO user (id, workspace_id, email, display_name, role, password_hash, \
         created_at) VALUES (?1, ?2, 'ada@izlek.sh', 'Ada', 'admin', 'x', '2026-08-26T00:00:00Z')",
        turso::params![admin.clone(), workspace.clone()],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO board (id, workspace_id, name, created_at) VALUES (?1, ?2, 'Board', \
         '2026-08-26T00:00:00Z')",
        turso::params![board.clone(), workspace.clone()],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO board_column (id, board_id, name, position, is_done) VALUES (?1, ?2, \
         'Backlog', 0, 0)",
        turso::params![backlog.clone(), board.clone()],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO board_column (id, board_id, name, position, is_done) VALUES (?1, ?2, \
         'Done', 1, 1)",
        turso::params![done.clone(), board.clone()],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO task (id, board_id, task_key, title, column_id, created_by, created_at, \
         updated_at) VALUES (?1, ?2, 'DZ-1', 'Ship it', ?3, ?4, '2026-08-26T00:00:00Z', '2026-08-26T00:00:00Z')",
        turso::params![task.clone(), board.clone(), done.clone(), admin.clone()],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO transition (id, task_id, from_column, to_column, actor_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, '2026-08-26T00:00:00Z')",
        turso::params![
            transition.clone(),
            task.clone(),
            backlog.clone(),
            done.clone(),
            admin.clone()
        ],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO mail_rule (id, board_id, trigger_kind, trigger_column, subject, audience, \
         enabled, created_at) VALUES (?1, ?2, 'status', ?3, 'Task completed', 'assignees', 1, \
         '2026-08-26T00:00:00Z')",
        turso::params![rule.clone(), board.clone(), done.clone()],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO mail_send (id, rule_id, event_id, task_id, recipient, state, attempts, \
         claimed_at) VALUES ('s1', ?1, ?2, ?3, 'ada@izlek.sh', 'pending', 0, '2026-08-26T00:00:00Z')",
        turso::params![rule.clone(), transition.clone(), task.clone()],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO mail_decision (id, rule_id, event_id, task_id, outcome, detail, \
         created_at) VALUES ('d1', ?1, ?2, ?3, 'owed', '', '2026-08-26T00:00:00Z')",
        turso::params![rule.clone(), transition.clone(), task.clone()],
    )
    .await
    .unwrap();
    drop(conn);

    (PathBuf::from(path), workspace, board, rule, task)
}

#[tokio::test]
async fn migrations_apply_once_and_survive_reopen() {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("izlek.db").to_string_lossy().into_owned();

    let first = TursoStore::open(&path).await.unwrap();
    assert_eq!(first.schema_version().await.unwrap(), 20);
    claim(&first).await;
    drop(first);

    // Re-opening must not re-run 0001 (which would fail on CREATE TABLE) and
    // must not lose what the first open wrote.
    let second = TursoStore::open(&path).await.unwrap();
    assert_eq!(second.schema_version().await.unwrap(), 20);
    assert_eq!(second.workspace().await.unwrap().unwrap().name, "Izlek");
    drop(second);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migration_0013_rebuilds_mail_rule_without_losing_its_ledger() {
    // `mail_send.rule_id` and `mail_decision.rule_id` both carry ON DELETE
    // CASCADE at `mail_rule`, and every connection this store opens runs with
    // foreign keys on. Rebuilding `mail_rule` — dropping the old table while
    // rows still point at it — would, without a guard, cascade the drop
    // straight through this rule's own send and its own decision.
    let (path, workspace, board, rule, task) =
        a_pre_0013_store_with_a_rule_send_and_decision().await;

    let store = TursoStore::open(path.to_str().unwrap()).await.unwrap();
    assert_eq!(store.schema_version().await.unwrap(), 20);
    assert_eq!(store.board(&workspace).await.unwrap().unwrap().id, board);

    let rules = store.mail_rules(&board).await.unwrap();
    assert_eq!(rules.len(), 1, "the rule survived the rebuild");
    assert_eq!(rules[0].id, rule);
    assert_eq!(rules[0].audience, Audience::Assignees);

    let sends = store.sends_for_rule(&rule, 10).await.unwrap();
    assert_eq!(sends.len(), 1, "the send survived, joined to its rule");
    assert_eq!(sends[0].task_id, Some(task.clone()));

    let decisions = store.recent_mail_decisions(10, izlek_core::store::FeedPage::Newest).await.unwrap();
    assert_eq!(
        decisions.len(),
        1,
        "the decision survived, joined to its rule"
    );
    assert_eq!(decisions[0].rule_id, rule);

    drop(store);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
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
        "Izlek"
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
                    "Izlek",
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
        task.await
            .unwrap()
            .expect("concurrent store access must never surface Misuse(\"concurrent use forbidden\")");
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
            "Izlek",
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
                from_name: "Izlek".into(),
                from_address: "izlek@izlek.sh".into(),
            },
        )
        .await
        .unwrap();

    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(ws.smtp_host.as_deref(), Some("smtp.fastmail.com"));
    assert_eq!(ws.smtp_port, Some(587));
    assert_eq!(ws.smtp_username.as_deref(), Some("izlek"));
    assert_eq!(ws.smtp_from_name.as_deref(), Some("Izlek"));
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
        from_name: "Izlek".into(),
        from_address: "izlek@izlek.sh".into(),
    };
    scratch.store.set_sender(&ws_id, sender.clone()).await.unwrap();

    sender.port = 465;
    sender.password = None;
    scratch.store.set_sender(&ws_id, sender).await.unwrap();

    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(ws.smtp_port, Some(465), "the edit did not land");
    assert!(ws.smtp_password_set, "the password was blanked by an edit");
    assert_eq!(
        scratch.store.smtp_password(&ws_id).await.unwrap().as_deref(),
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
        from_name: "Izlek".into(),
        from_address: "izlek@izlek.sh".into(),
    };
    scratch.store.set_sender(&ws_id, sender.clone()).await.unwrap();
    sender.password = Some("the-new-one".into());
    scratch.store.set_sender(&ws_id, sender).await.unwrap();

    assert_eq!(
        scratch.store.smtp_password(&ws_id).await.unwrap().as_deref(),
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
                from_name: "Izlek".into(),
                from_address: "izlek@izlek.sh".into(),
            },
        )
        .await
        .unwrap();

    let conn = raw_conn(&scratch).await;
    let mut rows = conn
        .query("SELECT smtp_password FROM workspace WHERE id = ?1", turso::params![ws_id.clone()])
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let column: String = row.get(0).unwrap();
    assert!(
        !column.contains("a-very-secret-string"),
        "the plaintext is sitting in the column: {column}"
    );
    assert!(column.starts_with("v1:"), "expected the sealed envelope, got: {column}");

    // And the read path still gets the real password back.
    assert_eq!(
        scratch.store.smtp_password(&ws_id).await.unwrap().as_deref(),
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
                from_name: "Izlek".into(),
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
                from_name: "Izlek".into(),
                from_address: "izlek@izlek.sh".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        scratch.store.smtp_password(&ws_id).await.unwrap().as_deref(),
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
        assert_eq!(mode, 0o600, "{} is not owner-only: {mode:o}", path.display());
    }
}

#[tokio::test]
async fn limits_round_trip_including_the_file_type_list() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let types = vec!["png".to_string(), "pdf".to_string()];
    scratch
        .store
        .set_limits(&ws_id, 10 * 1024 * 1024, 512 * 1024, &types)
        .await
        .unwrap();
    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(ws.attachment_limit_bytes, 10 * 1024 * 1024);
    assert_eq!(ws.photo_limit_bytes, 512 * 1024);
    assert_eq!(ws.allowed_file_types, types);
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
        assert!(store.user_by_email(&ws_id, "grace.new@izlek.sh").await.unwrap().is_some());
        assert!(store.user_by_email(&ws_id, "grace@izlek.sh").await.unwrap().is_none());

        // Taken by the admin already claimed above.
        let err = store.set_email(&user_id, &ws_id, "ada@izlek.sh").await.unwrap_err();
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
        .claim_workspace("Izlek", "ada@izlek.sh", "Ada", "$argon2id$fake")
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
        .claim_workspace("Izlek", "ada@izlek.sh", "Ada", "tide-tables-1892")
        .await
        .unwrap();
    (scratch, accounts, signed_in.user)
}

#[tokio::test]
async fn claiming_makes_an_admin_and_signs_them_in() {
    let (_scratch, accounts) = accounts().await;
    let (workspace, signed_in) = accounts
        .claim_workspace("Izlek", "ada@izlek.sh", "Ada", "tide-tables-1892")
        .await
        .unwrap();
    assert_eq!(workspace.name, "Izlek");
    assert_eq!(signed_in.user.role, Role::Admin);

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
        .claim_workspace("Izlek", "ada@izlek.sh", "Ada", "short")
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

    let queue = accounts.store().mail_queue(10, izlek_core::store::FeedPage::Newest).await.unwrap();
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

    let fresh = accounts
        .change_password(
            &admin.id,
            "tide-tables-1892",
            "chronometer-1761",
            "198.51.100.7",
        )
        .await
        .unwrap();

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
            board_id: &board.id,
            column_id: &column_id,
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

    let filter = ActivityFilter { actor: Some(admin.clone()), ..Default::default() };
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
        page = FeedPage::Before(FeedCursor { at: last.at, id: last.id.clone() });
        walked.extend(rows);
    }
    assert_eq!(walked.len(), 10);
    assert!(walked.iter().all(|r| r.actor_name.as_deref() == Some("Ada")));

    let preceding = store
        .count_activity_preceding(
            &filter,
            Dir::Newest,
            Some(&FeedCursor { at: walked[3].at, id: walked[3].id.clone() }),
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
    let Some(tail) = key.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('-')) else {
        return false;
    };
    (5..=7).contains(&tail.len())
        && tail.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
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
        assert!(is_task_key_shaped(key, "DZ"), "key {key} is not shaped like DZ-<5..7 chars>");
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
    let blocked_key = board.cards().find(|card| card.id == blocked).unwrap().task_key.clone();
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

    let big = CountingReads::new(store);
    let board = load(&big, &workspace).await.unwrap().unwrap();
    assert_eq!(board.task_count(), 40);
    assert_eq!(
        big.count(),
        6,
        "the round trips a board costs must not follow the number of tasks"
    );
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

    // The refusal wrote nothing.
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
            &admin,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert_eq!(ids, Vec::<String>::new(), "a save that changed nothing writes nothing");
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
    let kinds: Vec<&ActivityKind> = detail.activity.iter().map(|e| &e.kind).collect();
    assert_eq!(
        kinds,
        [
            &ActivityKind::Created,
            &ActivityKind::Retitled,
            &ActivityKind::Described,
            &ActivityKind::DeadlineSet
        ]
    );
}

#[tokio::test]
async fn a_task_detail_costs_eight_queries_whatever_it_carries() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let bare = add_task(store, &workspace, "Backlog", "bare", None, &admin).await;
    let counted = CountingDetail::new(store);
    load_detail(&counted, &workspace, &bare).await.unwrap();
    assert_eq!(counted.count(), 8, "a task with nothing hung off it");

    // Twenty comments, twenty activity lines, twenty people who could be
    // assigned and twenty tasks on the other end of a dependency — the four
    // things a naive detail query fans out on.
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
                &ActivityKind::Moved,
                "to Review",
                now,
            )
            .await
            .unwrap();
        let neighbour =
            add_task(store, &workspace, "Backlog", &format!("n{n}"), None, &admin).await;
        store.add_dependency(&neighbour, &heavy, now).await.unwrap();
    }

    let counted = CountingDetail::new(store);
    let detail = load_detail(&counted, &workspace, &heavy)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.comments.len(), 20);
    assert_eq!(detail.assignees.len(), 20);
    assert_eq!(detail.blocks.len(), 20);
    // Twenty comments (each its own Commented line), twenty moves, plus the
    // line create_task wrote.
    assert_eq!(detail.activity.len(), 41);
    assert_eq!(
        counted.count(),
        8,
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
            &Trigger::StatusBecomes(column_id),
            subject,
            Audience::Assignees,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_rule_that_fires_on_nothing_is_refused_by_the_schema() {
    // The store's own API cannot build a half-written rule, so this reaches
    // past it: a status rule with no column, straight at the table. The check
    // constraint is the guard, and if it were ever dropped this insert would
    // quietly succeed and the engine would carry a rule that matches nothing.
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
             VALUES ('r1', ?1, 'status', NULL, 'Task completed', 'assignees', 1, '2026-08-26')",
            turso::params![board.id.clone()],
        )
        .await;
    assert!(refused.is_err(), "a status rule with no column was stored");

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
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now)
        .await
        .unwrap();
    assert!(first.is_some(), "the first run owns the send");

    // The engine running a second time over the same crossing — a restart, a
    // retry sweep, two workers — must not mail Ada twice. Nothing is read
    // first: the insert loses.
    let second = scratch
        .store
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now)
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
                .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now)
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
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now)
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
        .claim_send(&rule.id, &transition.id, &task, "gone@izlek.sh", now)
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
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now)
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
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now)
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

use izlek_core::mail::{Engine, MailError, Mailer, Outgoing, backoff};
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

/// A store shared between the tests and an engine.
async fn shared() -> (PathBuf, Arc<TursoStore>, String, String) {
    let dir = std::env::temp_dir().join(format!("izlek-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(
        TursoStore::open(dir.join("izlek.db").to_str().unwrap())
            .await
            .unwrap(),
    );
    let (workspace, admin) = claim(&store).await;
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
            board_id: &board.id,
            column_id: &column_id,
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
            &Trigger::StatusBecomes(column_named(&store, &workspace, "Done").await),
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
    // just did is how a person learns to filter Izlek's mail away.
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

    let decisions = store.recent_mail_decisions(10, izlek_core::store::FeedPage::Newest).await.unwrap();
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

    let decisions = store.recent_mail_decisions(10, izlek_core::store::FeedPage::Newest).await.unwrap();
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

    let decisions = store.recent_mail_decisions(10, izlek_core::store::FeedPage::Newest).await.unwrap();
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
            &Trigger::StatusBecomes(column_id),
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
        .record_mail_decision(
            &rule.id,
            &transition.id,
            &task,
            MailOutcome::Owed,
            "",
            now,
        )
        .await
        .unwrap();

    scratch
        .store
        .update_mail_rule(&rule.id, &Trigger::Unblocked, "New subject", Audience::Board, false)
        .await
        .unwrap();

    let updated = scratch.store.mail_rule(&rule.id).await.unwrap().unwrap();
    assert_eq!(updated.id, rule.id);
    assert_eq!(updated.trigger, Trigger::Unblocked);
    assert_eq!(updated.subject, "New subject");
    assert_eq!(updated.audience, Audience::Board);
    assert_eq!(updated.enabled, rule.enabled);
    assert_eq!(updated.created_at, rule.created_at);

    let decisions = scratch.store.recent_mail_decisions(10, izlek_core::store::FeedPage::Newest).await.unwrap();
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
        creators.iter().map(|p| p.user_id.as_str()).collect::<Vec<_>>(),
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
    let bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00, 0xfe];
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
        .claim_send(&rule.id, &transition.id, &task, "pending@izlek.sh", now)
        .await
        .unwrap()
        .unwrap();

    let failed = store
        .claim_send(&rule.id, &transition.id, &task, "failed@izlek.sh", now)
        .await
        .unwrap()
        .unwrap();
    store
        .record_send_refused(&failed.id, "timeout", Some(now + Duration::minutes(5)), now)
        .await
        .unwrap();

    let sent = store
        .claim_send(&rule.id, &transition.id, &task, "sent@izlek.sh", now)
        .await
        .unwrap()
        .unwrap();
    store.record_send_accepted(&sent.id, now).await.unwrap();

    let abandoned = store
        .claim_send(&rule.id, &transition.id, &task, "abandoned@izlek.sh", now)
        .await
        .unwrap()
        .unwrap();
    store
        .record_send_refused(&abandoned.id, "bounced", None, now)
        .await
        .unwrap();

    let queue = store.mail_queue(10, izlek_core::store::FeedPage::Newest).await.unwrap();
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
        .queue_invite("newcomer@izlek.sh", "Join Izlek", "Come aboard.", now)
        .await
        .unwrap();
    assert_eq!(invite.rule_id, None);
    assert_eq!(invite.kind, SendKind::Invite);

    let owed = store.sends_owed(now, 10).await.unwrap();
    let found = owed.iter().find(|s| s.id == invite.id).unwrap();
    assert_eq!(found.rule_id, None);

    let queue = store.mail_queue(10, izlek_core::store::FeedPage::Newest).await.unwrap();
    let found = queue.iter().find(|s| s.id == invite.id).unwrap();
    assert_eq!(found.rule_id, None);
}

#[tokio::test]
async fn an_invite_mail_with_no_sender_is_held_not_failed() {
    let (dir, store, _workspace, _admin) = shared().await;
    let now = OffsetDateTime::now_utc();
    store
        .queue_invite("newcomer@izlek.sh", "Join Izlek", "Come aboard.", now)
        .await
        .unwrap();

    let mailer = Remembering::refusing(vec![MailError::unsent("no sender configured")]);
    let engine = Engine::new(store.clone(), mailer.clone(), "https://izlek.sh");
    let report = engine.deliver_owed(now, 10).await.unwrap();
    assert_eq!(report.held, 1);
    assert_eq!(report.sent, 0);

    let queue = store.mail_queue(10, izlek_core::store::FeedPage::Newest).await.unwrap();
    let held = queue
        .iter()
        .find(|s| s.recipient == "newcomer@izlek.sh")
        .unwrap();
    assert_eq!(held.attempts, 0);
    let _ = std::fs::remove_dir_all(&dir);
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
        .record_activity(&task_a, Some(&admin), &ActivityKind::Created, "", t0)
        .await
        .unwrap();
    store
        .record_activity(
            &task_b,
            Some(&other),
            &ActivityKind::Retitled,
            "new title",
            t0 + Duration::seconds(1),
        )
        .await
        .unwrap();

    let feed = store.recent_activity(10, izlek_core::store::FeedPage::Newest, izlek_core::store::Dir::Newest, &izlek_core::store::ActivityFilter::default()).await.unwrap();
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

    let whole = store.recent_activity(100, FeedPage::Newest, izlek_core::store::Dir::Newest, &izlek_core::store::ActivityFilter::default()).await.unwrap();
    assert_eq!(whole.len(), 23);

    let mut walked = Vec::new();
    let mut page = FeedPage::Newest;
    loop {
        let rows = store.recent_activity(7, page, izlek_core::store::Dir::Newest, &izlek_core::store::ActivityFilter::default()).await.unwrap();
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
        .record_activity(&task, Some(&admin), &ActivityKind::Created, "", t0)
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

    let feed = store.recent_activity(10, izlek_core::store::FeedPage::Newest, izlek_core::store::Dir::Newest, &izlek_core::store::ActivityFilter::default()).await.unwrap();
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

    let decisions = store.recent_mail_decisions(10, izlek_core::store::FeedPage::Newest).await.unwrap();
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

    let decisions = store.recent_mail_decisions(10, izlek_core::store::FeedPage::Newest).await.unwrap();
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
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now)
        .await
        .unwrap()
        .unwrap();
    store
        .claim_send(&rule.id, &other_transition.id, &other_task, "emre@izlek.sh", now)
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
        .claim_send(&rule.id, &transition.id, &task, "failed@izlek.sh", now)
        .await
        .unwrap()
        .unwrap();
    store
        .record_send_refused(&failed.id, "timeout", None, now)
        .await
        .unwrap();

    let sent = store
        .claim_send(&rule.id, &transition.id, &task, "sent@izlek.sh", now)
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
    assert_eq!(reread_sent.state, SendState::Sent, "a sent send is untouched");

    let owed = store.sends_owed(now, 10).await.unwrap();
    assert!(owed.iter().any(|s| s.id == failed.id), "the requeued send is now due");
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
    assert!(owed.is_empty(), "a delivered notice is no longer owed: {owed:?}");
    std::fs::remove_dir_all(dir).ok();
}
