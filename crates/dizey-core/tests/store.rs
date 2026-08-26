//! Integration tests for dizey-core: the storage boundary driven through the
//! Turso implementation, and the account flows on top of it.
//!
//! New integration tests belong in this file rather than a new `tests/*.rs`:
//! one test binary links and runs once.

use std::path::PathBuf;

use dizey_core::Role;
use dizey_core::auth::{Token, hash_password};
use dizey_core::store::{
    Audience, MailRule, NewUser, SendState, Store, StoreError, Trigger, TursoStore, User,
};
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
    let (ws, admin) = scratch
        .store
        .claim_workspace(
            "Dizey",
            "ada@dizey.sh",
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
            "Dizey",
            "ada@dizey.sh",
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
        })
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn migrations_apply_once_and_survive_reopen() {
    let dir = std::env::temp_dir().join(format!("dizey-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dizey.db").to_string_lossy().into_owned();

    let first = TursoStore::open(&path).await.unwrap();
    assert_eq!(first.schema_version().await.unwrap(), 4);
    claim(&first).await;
    drop(first);

    // Re-opening must not re-run 0001 (which would fail on CREATE TABLE) and
    // must not lose what the first open wrote.
    let second = TursoStore::open(&path).await.unwrap();
    assert_eq!(second.schema_version().await.unwrap(), 4);
    assert_eq!(second.workspace().await.unwrap().unwrap().name, "Dizey");
    drop(second);
    let _ = std::fs::remove_dir_all(&dir);
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
        "Dizey"
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
    let dir = std::env::temp_dir().join(format!("dizey-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dizey.db").to_string_lossy().into_owned();
    let store = std::sync::Arc::new(TursoStore::open(&path).await.unwrap());

    let hash = hash_password("tide-tables-1892").unwrap();
    let mut claims = Vec::new();
    for i in 0..4 {
        let store = store.clone();
        let hash = hash.clone();
        claims.push(tokio::spawn(async move {
            store
                .claim_workspace(
                    "Dizey",
                    &format!("claimant{i}@dizey.sh"),
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

#[tokio::test]
async fn workspace_defaults_match_the_settings_screen() {
    let scratch = Scratch::open().await;
    let (ws, _) = scratch
        .store
        .claim_workspace(
            "Dizey",
            "ada@dizey.sh",
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
    assert!(ws.smtp_host.is_none());
}

#[tokio::test]
async fn smtp_password_is_never_part_of_the_workspace_record() {
    let (scratch, ws_id, _) = workspace_with_admin().await;
    scratch
        .store
        .set_smtp(
            &ws_id,
            "smtp.fastmail.com",
            465,
            "dizey",
            "hunter2",
            "Dizey",
            "dizey@dizey.sh",
        )
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
        scratch
            .store
            .smtp_password(&ws_id)
            .await
            .unwrap()
            .as_deref(),
        Some("hunter2")
    );
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
    assert_eq!(users.iter().filter(|u| u.role == Role::Viewer).count(), 1);
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
    scratch
        .store
        .set_role(&user.id, Role::Viewer)
        .await
        .unwrap();
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
    scratch
        .store
        .set_profile(&user.id, "Grace H.", None)
        .await
        .unwrap();
    assert!(
        scratch
            .store
            .user(&user.id)
            .await
            .unwrap()
            .unwrap()
            .photo_path
            .is_none()
    );
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
    store
        .claim_workspace("Dizey", "ada@dizey.sh", "Ada", "$argon2id$fake")
        .await
        .unwrap();
    assert!(store.workspace().await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prefetched_link_is_consumed_exactly_once() {
    let dir = std::env::temp_dir().join(format!("dizey-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dizey.db").to_string_lossy().into_owned();
    let store = std::sync::Arc::new(TursoStore::open(&path).await.unwrap());
    let (ws_id, _) = claim(&store).await;
    let user_id = member(&store, &ws_id, "grace@dizey.sh", "Grace").await;

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
    let user_id = member(&scratch.store, &ws_id, "grace@dizey.sh", "Grace").await;

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
    let user_id = member(&scratch.store, &ws_id, "grace@dizey.sh", "Grace").await;
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
    let user_id = member(&scratch.store, &ws_id, "grace@dizey.sh", "Grace").await;
    let other_id = member(&scratch.store, &ws_id, "linus@dizey.sh", "Linus").await;
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
            .record_auth_attempt("grace@dizey.sh", now - Duration::minutes(minutes_ago))
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
            .count_auth_attempts("grace@dizey.sh", window)
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
            .count_auth_attempts("nobody@dizey.sh", window)
            .await
            .unwrap(),
        0
    );

    // A success clears the bucket that succeeded, and only that one.
    scratch
        .store
        .clear_auth_attempts("grace@dizey.sh")
        .await
        .unwrap();
    assert_eq!(
        scratch
            .store
            .count_auth_attempts("grace@dizey.sh", window)
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
        .record_auth_attempt("grace@dizey.sh", now - Duration::hours(2))
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

use dizey_core::accounts::{AccountError, Accounts, RATE_LIMIT, SIGNIN_LINK_LIFETIME};
use dizey_core::auth::PasswordProblem;
use std::sync::Arc;

/// An accounts service over a throwaway database. The directory is kept alive
/// by the returned guard.
async fn accounts() -> (Scratch, Accounts) {
    let scratch = Scratch::open().await;
    let store = TursoStore::open(scratch.dir.join("dizey.db").to_str().unwrap())
        .await
        .unwrap();
    let accounts = Accounts::new(Arc::new(store) as Arc<dyn Store>);
    (scratch, accounts)
}

async fn claimed() -> (Scratch, Accounts, User) {
    let (scratch, accounts) = accounts().await;
    let (_, signed_in) = accounts
        .claim_workspace("Dizey", "ada@dizey.sh", "Ada", "tide-tables-1892")
        .await
        .unwrap();
    (scratch, accounts, signed_in.user)
}

#[tokio::test]
async fn claiming_makes_an_admin_and_signs_them_in() {
    let (_scratch, accounts) = accounts().await;
    let (workspace, signed_in) = accounts
        .claim_workspace("Dizey", "ada@dizey.sh", "Ada", "tide-tables-1892")
        .await
        .unwrap();
    assert_eq!(workspace.name, "Dizey");
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
        .claim_workspace("Dizey", "ada@dizey.sh", "Ada", "short")
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
        .invite(&admin, "grace@dizey.sh", "Grace", Role::Member)
        .await
        .unwrap();
    assert!(!invitation.user.has_signed_in(), "no password yet");
    assert!(
        invitation.expires_at
            > OffsetDateTime::now_utc() + SIGNIN_LINK_LIFETIME - Duration::minutes(1)
    );

    // The admin cannot sign in as them in the meantime.
    let as_them = accounts
        .sign_in("grace@dizey.sh", "tide-tables-1892", "198.51.100.7")
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
        .sign_in("grace@dizey.sh", "sextant-and-chart", "198.51.100.7")
        .await
        .unwrap();
}

#[tokio::test]
async fn only_an_admin_may_invite() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@dizey.sh", "Grace", Role::Member)
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
        .invite(&member, "linus@dizey.sh", "Linus", Role::Member)
        .await;
    assert!(matches!(attempt, Err(AccountError::Forbidden)));

    let mut viewer = member.clone();
    viewer.role = Role::Viewer;
    assert!(matches!(
        accounts
            .invite(&viewer, "linus@dizey.sh", "Linus", Role::Viewer)
            .await,
        Err(AccountError::Forbidden)
    ));
}

#[tokio::test]
async fn a_link_works_once_and_a_wrong_one_never_does() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@dizey.sh", "Grace", Role::Member)
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
        .sign_in("grace@dizey.sh", "sextant-and-chart", "198.51.100.7")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_rejected_password_does_not_burn_the_invitation() {
    let (_scratch, accounts, admin) = claimed().await;
    let invitation = accounts
        .invite(&admin, "grace@dizey.sh", "Grace", Role::Member)
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
        .invite(&admin, "grace@dizey.sh", "Grace", Role::Member)
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
        .sign_in("nobody@dizey.sh", "tide-tables-1892", "198.51.100.7")
        .await;
    let wrong = accounts
        .sign_in("ada@dizey.sh", "not-her-password", "198.51.100.8")
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
        .sign_in("ada@dizey.sh", "not-her-password", "198.51.100.7")
        .await;
    let hit_cost = baseline.elapsed();

    let started = std::time::Instant::now();
    let _ = accounts
        .sign_in("nobody@dizey.sh", "not-a-password", "198.51.100.8")
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
        .invite(&admin, "grace@dizey.sh", "Grace", Role::Member)
        .await
        .unwrap();
    // Not "set your password first" — that would confirm the address exists.
    assert!(matches!(
        accounts.sign_in("grace@dizey.sh", "", "198.51.100.7").await,
        Err(AccountError::Rejected)
    ));
}

#[tokio::test]
async fn sign_in_attempts_are_rate_limited_per_address() {
    let (_scratch, accounts, _admin) = claimed().await;
    for _ in 0..RATE_LIMIT {
        let _ = accounts
            .sign_in("ada@dizey.sh", "wrong", "198.51.100.7")
            .await;
    }
    // The next attempt is refused before any Argon2 work happens.
    assert!(matches!(
        accounts
            .sign_in("ada@dizey.sh", "wrong", "203.0.113.9")
            .await,
        Err(AccountError::RateLimited)
    ));
    // A different address from a fresh client is unaffected.
    assert!(matches!(
        accounts
            .sign_in("someone@dizey.sh", "wrong", "203.0.113.9")
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
            .sign_in("ada@dizey.sh", "wrong", &format!("203.0.113.{i}"))
            .await;
    }
    // The owner, from a client of their own, still gets in.
    accounts
        .sign_in("ada@dizey.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
}

/// The client bucket is the one that caps the Argon2 work, and it does refuse.
#[tokio::test]
async fn sign_in_attempts_are_rate_limited_per_client() {
    let (_scratch, accounts, _admin) = claimed().await;
    for i in 0..RATE_LIMIT {
        let _ = accounts
            .sign_in(&format!("nobody{i}@dizey.sh"), "wrong", "203.0.113.9")
            .await;
    }
    // Refused before any Argon2 work, whatever address it asks about.
    assert!(matches!(
        accounts
            .sign_in("ada@dizey.sh", "tide-tables-1892", "203.0.113.9")
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
        .invite(&admin, "grace@dizey.sh", "Grace", Role::Member)
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
            .sign_in("ada@dizey.sh", "wrong", "198.51.100.7")
            .await;
    }
    accounts
        .sign_in("ada@dizey.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
    // Someone who mistypes and then gets it right is not left at the edge.
    assert!(matches!(
        accounts
            .sign_in("ada@dizey.sh", "wrong", "198.51.100.7")
            .await,
        Err(AccountError::Rejected)
    ));
}

#[tokio::test]
async fn changing_a_password_signs_out_every_device() {
    let (_scratch, accounts, admin) = claimed().await;
    let first = accounts
        .sign_in("ada@dizey.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
    let second = accounts
        .sign_in("ada@dizey.sh", "tide-tables-1892", "198.51.100.8")
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
        .sign_in("ada@dizey.sh", "chronometer-1761", "198.51.100.7")
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
        .sign_in("ada@dizey.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
}

#[tokio::test]
async fn signing_out_ends_that_browser_only() {
    let (_scratch, accounts, _admin) = claimed().await;
    let laptop = accounts
        .sign_in("ada@dizey.sh", "tide-tables-1892", "198.51.100.7")
        .await
        .unwrap();
    let phone = accounts
        .sign_in("ada@dizey.sh", "tide-tables-1892", "198.51.100.8")
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
        .invite(&admin, "grace@dizey.sh", "Grace", Role::Member)
        .await
        .unwrap();
    assert!(matches!(
        accounts
            .invite(&admin, "GRACE@dizey.sh", "Grace again", Role::Member)
            .await,
        Err(AccountError::AddressTaken)
    ));
    assert!(matches!(
        accounts
            .invite(&admin, "ada@dizey.sh", "Ada again", Role::Member)
            .await,
        Err(AccountError::AddressTaken)
    ));
}

// -- board ------------------------------------------------------------------

use dizey_core::board::{BoardReads, BoardView, Moved, Person, load};
use dizey_core::store::NewTask;
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
    ) -> Result<Option<dizey_core::board::BoardMeta>, StoreError> {
        self.tick();
        self.inner.board(workspace_id).await
    }

    async fn columns(&self, board_id: &str) -> Result<Vec<dizey_core::board::Column>, StoreError> {
        self.tick();
        self.inner.columns(board_id).await
    }

    async fn tasks_for_board(
        &self,
        board_id: &str,
    ) -> Result<Vec<dizey_core::board::TaskRow>, StoreError> {
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
        .id
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

#[tokio::test]
async fn tasks_get_consecutive_keys_off_the_board_counter() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    for title in ["Pricing page draft", "Choose analytics stack"] {
        add_task(store, &workspace, "Backlog", title, None, &admin).await;
    }

    let board = board_of(store, &workspace).await;
    let keys: Vec<&str> = board.cards().map(|card| card.task_key.as_str()).collect();
    assert_eq!(keys, ["DZ-01", "DZ-02"]);
}

#[tokio::test]
async fn a_card_carries_its_assignees_comments_and_dependency_keys() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let mel = store
        .create_user(NewUser {
            workspace_id: workspace.clone(),
            email: "mel@dizey.sh".into(),
            display_name: "Mel Duarte".into(),
            role: Role::Member,
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
    let card = board.cards().find(|card| card.id == blocking).unwrap();
    assert_eq!(card.assignees.len(), 2);
    assert_eq!(card.comment_count, 3);
    assert_eq!(card.blocks, ["DZ-02"]);
    assert!(card.blocked_by.is_empty());
    assert!(!card.is_blocked());

    let waiting = board.cards().find(|card| card.id == blocked).unwrap();
    assert_eq!(waiting.blocked_by, ["DZ-01"]);
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
        photo_path: None,
    };
    assert_eq!(person("Mel Duarte").initials(), "MD");
    assert_eq!(person("Ada").initials(), "A");
    assert_eq!(person("  ").initials(), "?");
}

// -- task detail ------------------------------------------------------------

use dizey_core::detail::{
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
    ) -> Result<Vec<dizey_core::Column>, StoreError> {
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
            email: "quiet@dizey.sh".into(),
            display_name: "Quiet Reader".into(),
            role: Role::Viewer,
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

    let unblocked = store.delete_task(&blocking, &admin, now).await.unwrap();
    assert_eq!(
        unblocked,
        vec![freed.clone()],
        "only the one with nothing else in front of it"
    );

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

    store
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

    store
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
async fn a_task_detail_costs_seven_queries_whatever_it_carries() {
    let (scratch, workspace, admin) = workspace_with_admin().await;
    let store = &scratch.store;
    let now = OffsetDateTime::now_utc();

    let bare = add_task(store, &workspace, "Backlog", "bare", None, &admin).await;
    let counted = CountingDetail::new(store);
    load_detail(&counted, &workspace, &bare).await.unwrap();
    assert_eq!(counted.count(), 7, "a task with nothing hung off it");

    // Twenty comments, twenty activity lines, twenty people who could be
    // assigned and twenty tasks on the other end of a dependency — the four
    // things a naive detail query fans out on.
    let heavy = add_task(store, &workspace, "Backlog", "heavy", None, &admin).await;
    for n in 0..20 {
        let person = store
            .create_user(NewUser {
                workspace_id: workspace.clone(),
                email: format!("member{n}@dizey.sh"),
                display_name: format!("Member {n}"),
                role: Role::Member,
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
    // Twenty moves plus the line create_task wrote.
    assert_eq!(detail.activity.len(), 21);
    assert_eq!(
        counted.count(),
        7,
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
        store.delete_task(&doomed, &admin, now).await.unwrap(),
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
            email: "orphan@dizey.sh".into(),
            display_name: "Orphan".into(),
            role: Role::Member,
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

    let dir = std::env::temp_dir().join(format!("dizey-fk-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dizey.db").to_str().unwrap().to_owned();
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
             VALUES ('u-orphan', 'no-such-workspace', 'orphan@dizey.sh', 'Orphan', \
             'member', '2026-08-26T00:00:00Z')",
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
) -> dizey_core::board::Transition {
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
    let dir = std::env::temp_dir().join(format!("dizey-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dizey.db").to_string_lossy().into_owned();
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
        )
        .await
        .expect("an unblocked rule is whole without a column");

    let written = scratch.store.mail_rules(&board.id).await.unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].trigger, Trigger::Unblocked);
    assert!(written[0].enabled, "a new rule is live");
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
        .claim_send(&rule.id, &transition.id, &task, "ada@dizey.sh", now)
        .await
        .unwrap();
    assert!(first.is_some(), "the first run owns the send");

    // The engine running a second time over the same crossing — a restart, a
    // retry sweep, two workers — must not mail Ada twice. Nothing is read
    // first: the insert loses.
    let second = scratch
        .store
        .claim_send(&rule.id, &transition.id, &task, "ada@dizey.sh", now)
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
                .claim_send(&rule.id, &transition.id, &task, "ada@dizey.sh", now)
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
        .claim_send(&rule.id, &transition.id, &task, "ada@dizey.sh", now)
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
        .claim_send(&rule.id, &transition.id, &task, "gone@dizey.sh", now)
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
        .claim_send(&rule.id, &transition.id, &task, "ada@dizey.sh", now)
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
            email: "viewer@dizey.sh".into(),
            display_name: "Vera".into(),
            role: Role::Viewer,
        })
        .await
        .unwrap();
    let mate = member(&scratch.store, &workspace, "emre@dizey.sh", "Emre").await;
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
            list.iter().all(|person| person.email != "viewer@dizey.sh"),
            "a Viewer was about to be mailed: {list:?}"
        );
    }
    let assignees = scratch.store.recipients_for_task(&task).await.unwrap();
    assert_eq!(
        assignees
            .iter()
            .map(|p| p.email.as_str())
            .collect::<Vec<_>>(),
        ["emre@dizey.sh"]
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
        .claim_send(&rule.id, &transition.id, &task, "ada@dizey.sh", now)
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
