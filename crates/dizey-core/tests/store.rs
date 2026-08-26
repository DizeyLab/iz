//! Behaviour of the storage boundary, driven through the Turso implementation.
//!
//! New storage tests belong in this file rather than a new `tests/*.rs`: one
//! test binary links and runs once.

use std::path::PathBuf;

use dizey_core::store::{DeletePolicy, NewUser, Store, StoreError, TursoStore};
use dizey_core::Role;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// A throwaway database on disk. Turso's in-memory mode is not what production
/// runs, so the tests exercise a real file.
struct Scratch {
    dir: PathBuf,
    store: TursoStore,
}

impl Scratch {
    async fn open() -> Self {
        let dir = std::env::temp_dir().join(format!("dizey-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TursoStore::open(dir.join("dizey.db").to_str().unwrap())
            .await
            .unwrap();
        Self { dir, store }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn workspace_with_admin() -> (Scratch, String, String) {
    let scratch = Scratch::open().await;
    let ws = scratch.store.create_workspace("Dizey").await.unwrap();
    let admin = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws.id.clone(),
            email: "ada@dizey.sh".into(),
            display_name: "Ada".into(),
            role: Role::Admin,
        })
        .await
        .unwrap();
    (scratch, ws.id, admin.id)
}

#[tokio::test]
async fn migrations_apply_once_and_survive_reopen() {
    let dir = std::env::temp_dir().join(format!("dizey-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dizey.db").to_string_lossy().into_owned();

    let first = TursoStore::open(&path).await.unwrap();
    assert_eq!(first.schema_version().await.unwrap(), 1);
    first.create_workspace("Dizey").await.unwrap();
    drop(first);

    // Re-opening must not re-run 0001 (which would fail on CREATE TABLE) and
    // must not lose what the first open wrote.
    let second = TursoStore::open(&path).await.unwrap();
    assert_eq!(second.schema_version().await.unwrap(), 1);
    assert_eq!(second.workspace().await.unwrap().unwrap().name, "Dizey");
    drop(second);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_database_holds_exactly_one_workspace() {
    let scratch = Scratch::open().await;
    assert!(scratch.store.workspace().await.unwrap().is_none());
    scratch.store.create_workspace("Dizey").await.unwrap();
    let again = scratch.store.create_workspace("Other").await;
    assert!(matches!(again, Err(StoreError::Conflict("workspace"))));
}

#[tokio::test]
async fn workspace_defaults_match_the_settings_screen() {
    let scratch = Scratch::open().await;
    let ws = scratch.store.create_workspace("Dizey").await.unwrap();
    assert_eq!(ws.attachment_limit_bytes, 25 * 1024 * 1024);
    assert_eq!(ws.photo_limit_bytes, 2 * 1024 * 1024);
    assert!(ws.allowed_file_types.is_empty(), "every type until narrowed");
    assert_eq!(ws.who_can_delete_tasks, DeletePolicy::Anyone);
    assert!(ws.smtp_host.is_none());
}

#[tokio::test]
async fn smtp_password_is_never_part_of_the_workspace_record() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    scratch
        .store
        .set_smtp(&ws_id, "smtp.fastmail.com", 465, "dizey", "hunter2", "Dizey", "dizey@dizey.sh")
        .await
        .unwrap();

    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(ws.smtp_host.as_deref(), Some("smtp.fastmail.com"));
    assert_eq!(ws.smtp_port, Some(465));
    assert_eq!(ws.smtp_from_address.as_deref(), Some("dizey@dizey.sh"));
    // The only way to the password is the mailer's own call.
    let serialised = serde_json::to_string(&ws).unwrap();
    assert!(!serialised.contains("hunter2"), "{serialised}");
    assert_eq!(
        scratch.store.smtp_password(&ws_id).await.unwrap().as_deref(),
        Some("hunter2")
    );
}

#[tokio::test]
async fn limits_round_trip_including_the_file_type_list() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let types = vec!["png".to_string(), "pdf".to_string()];
    scratch
        .store
        .set_limits(&ws_id, 10 * 1024 * 1024, 512 * 1024, &types, DeletePolicy::Admin)
        .await
        .unwrap();
    let ws = scratch.store.workspace().await.unwrap().unwrap();
    assert_eq!(ws.attachment_limit_bytes, 10 * 1024 * 1024);
    assert_eq!(ws.photo_limit_bytes, 512 * 1024);
    assert_eq!(ws.allowed_file_types, types);
    assert_eq!(ws.who_can_delete_tasks, DeletePolicy::Admin);
}

#[tokio::test]
async fn an_invited_member_has_no_password_until_they_choose_one() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let member = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id.clone(),
            email: "grace@dizey.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
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
            email: "  ADA@Dizey.sh ".into(),
            display_name: "Ada again".into(),
            role: Role::Member,
        })
        .await;
    assert!(matches!(dup, Err(StoreError::Conflict("account"))));
    assert!(
        scratch
            .store
            .user_by_email(&ws_id, "Ada@DIZEY.sh")
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
            .user_by_email(&ws_id, "nobody@dizey.sh")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn members_list_and_count_for_the_admin_screen() {
    let (scratch, ws_id, admin_id) = workspace_with_admin().await;
    for (email, name, role) in [
        ("grace@dizey.sh", "Grace", Role::Member),
        ("linus@dizey.sh", "Linus", Role::Viewer),
    ] {
        scratch
            .store
            .create_user(NewUser {
                workspace_id: ws_id.clone(),
                email: email.into(),
                display_name: name.into(),
                role,
            })
            .await
            .unwrap();
    }
    assert_eq!(scratch.store.count_users(&ws_id).await.unwrap(), 3);
    let users = scratch.store.users(&ws_id).await.unwrap();
    assert_eq!(users[0].id, admin_id);
    assert_eq!(
        users.iter().filter(|u| u.role == Role::Viewer).count(),
        1
    );
}

#[tokio::test]
async fn profile_and_role_updates_stick() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let user = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id,
            email: "grace@dizey.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
        })
        .await
        .unwrap();

    scratch
        .store
        .set_profile(&user.id, "Grace H.", Some("photos/grace.png"))
        .await
        .unwrap();
    scratch.store.set_role(&user.id, Role::Viewer).await.unwrap();
    let at = OffsetDateTime::now_utc();
    scratch.store.mark_signed_in(&user.id, at).await.unwrap();

    let user = scratch.store.user(&user.id).await.unwrap().unwrap();
    assert_eq!(user.display_name, "Grace H.");
    assert_eq!(user.photo_path.as_deref(), Some("photos/grace.png"));
    assert_eq!(user.role, Role::Viewer);
    // Stored as RFC 3339 text, so equality holds to the second.
    assert_eq!(
        user.last_signed_in_at.unwrap().unix_timestamp(),
        at.unix_timestamp()
    );

    // Clearing the photo is a real update, not a no-op.
    scratch.store.set_profile(&user.id, "Grace H.", None).await.unwrap();
    assert!(scratch.store.user(&user.id).await.unwrap().unwrap().photo_path.is_none());
}

#[tokio::test]
async fn updates_to_a_missing_user_are_not_found() {
    let scratch = Scratch::open().await;
    let missing = Uuid::new_v4().to_string();
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
            email: "grace@dizey.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
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

    scratch.store.consume_signin_link(&link.id, now).await.unwrap();
    let used = scratch
        .store
        .signin_link_by_hash("hash-of-the-token")
        .await
        .unwrap()
        .unwrap();
    assert!(!used.is_usable(now));
    // A second use finds nothing left to consume.
    assert!(matches!(
        scratch.store.consume_signin_link(&link.id, now).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn an_expired_link_is_still_a_live_account() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    let user = scratch
        .store
        .create_user(NewUser {
            workspace_id: ws_id,
            email: "grace@dizey.sh".into(),
            display_name: "Grace".into(),
            role: Role::Member,
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
    store.create_workspace("Dizey").await.unwrap();
    assert!(store.workspace().await.unwrap().is_some());
}
