//! Storage boundary.
//!
//! Everything the app does to persistent state goes through [`Store`]. The only
//! implementation today is Turso (in-process, SQLite-compatible); a Postgres
//! swap is a new impl of this trait and nothing else.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::board::{BoardReads, TaskRow};
use crate::detail::{ActivityKind, DetailReads};
use crate::Role;

pub mod turso_store;

pub use turso_store::TursoStore;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("this workspace already has an owner")]
    AlreadyClaimed,
    #[error("database: {0}")]
    Backend(String),
    #[error("not found")]
    NotFound,
    #[error("{0} already exists")]
    Conflict(&'static str),
    #[error("stored value is not valid: {0}")]
    Corrupt(String),
    /// The dependency asked for would put a task behind itself.
    #[error("that link would make a circle")]
    Cycle,
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A workspace and the settings that ride on it. The SMTP password is
/// deliberately absent: it is written through [`Store::set_smtp`] and read only
/// by the mailer, never returned to a page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub created_at: OffsetDateTime,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u32>,
    pub smtp_username: Option<String>,
    pub smtp_from_name: Option<String>,
    pub smtp_from_address: Option<String>,
    pub attachment_limit_bytes: u64,
    pub photo_limit_bytes: u64,
    pub allowed_file_types: Vec<String>,
    pub who_can_delete_tasks: DeletePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletePolicy {
    /// Anyone who can write tasks may delete one.
    Anyone,
    /// Only the admin.
    Admin,
}

impl DeletePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            DeletePolicy::Anyone => "anyone",
            DeletePolicy::Admin => "admin",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "anyone" => Ok(DeletePolicy::Anyone),
            "admin" => Ok(DeletePolicy::Admin),
            other => Err(StoreError::Corrupt(format!("delete policy {other:?}"))),
        }
    }
}

/// An account. `password_hash` is `None` for an invited member who has not
/// signed in yet — the admin creates the account with a name and an address and
/// can never read or set the password.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub workspace_id: String,
    pub email: String,
    pub display_name: String,
    pub role: Role,
    pub password_hash: Option<String>,
    pub photo_path: Option<String>,
    pub created_at: OffsetDateTime,
    pub last_signed_in_at: Option<OffsetDateTime>,
}

impl User {
    /// True once the person has chosen their own password.
    pub fn has_signed_in(&self) -> bool {
        self.password_hash.is_some()
    }
}

pub struct NewUser {
    pub workspace_id: String,
    pub email: String,
    pub display_name: String,
    pub role: Role,
}

/// A first-sign-in link. Only the hash of the token is ever stored; the
/// plaintext is shown once, when the link is created or resent.
#[derive(Debug, Clone, PartialEq)]
pub struct SigninLink {
    pub id: String,
    pub user_id: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub used_at: Option<OffsetDateTime>,
}

impl SigninLink {
    pub fn is_usable(&self, now: OffsetDateTime) -> bool {
        self.used_at.is_none() && now < self.expires_at
    }
}

/// A signed-in browser. As with every other token, only the hash of the cookie
/// value is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub revoked_at: Option<OffsetDateTime>,
}

impl Session {
    pub fn is_live(&self, now: OffsetDateTime) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// A task as it is written. The key (`DZ-14`) is not here: the store hands it
/// out from the board's counter, so two tasks created at once cannot collide.
pub struct NewTask<'a> {
    pub board_id: &'a str,
    pub column_id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub deadline: Option<time::Date>,
    pub created_by: &'a str,
}

/// The storage boundary. Dyn-safe on purpose: handlers hold `Arc<dyn Store>`.
///
/// The board's reads live in [`BoardReads`], so the sweep-per-board shape is a
/// trait a test can wrap and count.
#[async_trait]
pub trait Store: BoardReads + DetailReads + 'static {
    // -- workspace ---------------------------------------------------------

    /// Claims an empty database: writes the workspace, its first account and
    /// the single owner row in one transaction.
    ///
    /// The owner row has a fixed primary key, so two requests racing to claim
    /// the same empty workspace cannot both win, and the loser gets
    /// [`StoreError::AlreadyClaimed`] rather than quietly joining as a member.
    async fn claim_workspace(
        &self,
        workspace_name: &str,
        email: &str,
        display_name: &str,
        password_hash: &str,
    ) -> Result<(Workspace, User)>;

    /// The account that claimed the workspace, if it has been claimed.
    async fn owner(&self) -> Result<Option<User>>;

    async fn workspace(&self) -> Result<Option<Workspace>>;

    async fn set_smtp(
        &self,
        workspace_id: &str,
        host: &str,
        port: u32,
        username: &str,
        password: &str,
        from_name: &str,
        from_address: &str,
    ) -> Result<()>;

    /// Reads the sender password. Only the mailer calls this.
    async fn smtp_password(&self, workspace_id: &str) -> Result<Option<String>>;

    async fn set_limits(
        &self,
        workspace_id: &str,
        attachment_limit_bytes: u64,
        photo_limit_bytes: u64,
        allowed_file_types: &[String],
        who_can_delete_tasks: DeletePolicy,
    ) -> Result<()>;

    // -- users -------------------------------------------------------------

    async fn create_user(&self, new: NewUser) -> Result<User>;

    async fn user(&self, id: &str) -> Result<Option<User>>;

    /// Lookup by address. Callers must not turn a `None` into a different
    /// public response than a `Some`: the sign-in surface never reveals whether
    /// an address has an account.
    async fn user_by_email(&self, workspace_id: &str, email: &str) -> Result<Option<User>>;

    async fn users(&self, workspace_id: &str) -> Result<Vec<User>>;

    async fn count_users(&self, workspace_id: &str) -> Result<u64>;

    async fn set_password_hash(&self, user_id: &str, hash: &str) -> Result<()>;

    async fn set_profile(
        &self,
        user_id: &str,
        display_name: &str,
        photo_path: Option<&str>,
    ) -> Result<()>;

    async fn set_role(&self, user_id: &str, role: Role) -> Result<()>;

    async fn mark_signed_in(&self, user_id: &str, at: OffsetDateTime) -> Result<()>;

    // -- sign-in links -----------------------------------------------------

    /// Stores the hash of a freshly minted link. The caller keeps the plaintext
    /// and shows it once.
    async fn create_signin_link(
        &self,
        user_id: &str,
        token_hash: &str,
        expires_at: OffsetDateTime,
    ) -> Result<SigninLink>;

    /// Looks a link up by the hash of the presented token. Returns the link
    /// whether or not it is still usable, so the caller can tell an expired
    /// link apart from a wrong one — an expired link is not a dead account.
    async fn signin_link_by_hash(&self, token_hash: &str) -> Result<Option<SigninLink>>;

    /// Marks the link used, in a transaction, conditional on it still being
    /// unused, and reports whether this call is the one that consumed it.
    ///
    /// Mail clients prefetch links, so two redemptions of the same link is
    /// ordinary traffic rather than an attack: exactly one of them must win.
    async fn consume_signin_link(&self, id: &str, at: OffsetDateTime) -> Result<bool>;

    // -- sessions ----------------------------------------------------------

    async fn create_session(
        &self,
        user_id: &str,
        token_hash: &str,
        expires_at: OffsetDateTime,
    ) -> Result<Session>;

    /// Looks a session up by the hash of the cookie value. Returns it whether
    /// or not it is still live; the caller decides.
    async fn session_by_hash(&self, token_hash: &str) -> Result<Option<Session>>;

    /// The stored digest for a session, so the caller can compare it in
    /// constant time rather than trusting the index lookup alone.
    async fn session_token_hash(&self, id: &str) -> Result<Option<String>>;

    async fn revoke_session(&self, id: &str, at: OffsetDateTime) -> Result<()>;

    /// Signs out every browser this account has. This is what the change-
    /// password pane promises.
    async fn revoke_sessions_for_user(&self, user_id: &str, at: OffsetDateTime) -> Result<u64>;

    // -- rate limiting -----------------------------------------------------

    /// Records one attempt against a bucket — an address, or a client address.
    async fn record_auth_attempt(&self, bucket: &str, at: OffsetDateTime) -> Result<()>;

    /// How many attempts that bucket has made since `since`.
    async fn count_auth_attempts(&self, bucket: &str, since: OffsetDateTime) -> Result<u64>;

    /// Forgets a bucket's attempts. Called on a success, so a person who
    /// mistypes twice and then gets it right is not left near the limit.
    async fn clear_auth_attempts(&self, bucket: &str) -> Result<()>;

    /// Drops attempt rows older than `before`, so the ledger does not grow
    /// without bound.
    async fn prune_auth_attempts(&self, before: OffsetDateTime) -> Result<u64>;

    // -- board -------------------------------------------------------------

    /// Adds a column to the end of a board.
    async fn create_column(
        &self,
        board_id: &str,
        name: &str,
        position: i64,
        is_done: bool,
    ) -> Result<crate::board::Column>;

    /// Writes a task and hands it the next key on its board, in one
    /// transaction, so two writers cannot both take `DZ-14`.
    async fn create_task(&self, new: NewTask<'_>) -> Result<TaskRow>;

    /// Idempotent: assigning someone twice is not an error.
    async fn assign_task(&self, task_id: &str, user_id: &str) -> Result<()>;

    async fn unassign_task(&self, task_id: &str, user_id: &str) -> Result<()>;

    /// `blocked` cannot start until `blocking` is finished.
    ///
    /// Refuses with [`StoreError::Cycle`] if the edge would close a loop. The
    /// check runs inside the same transaction as the insert, so two writers
    /// racing to add the two halves of a circle cannot both pass a check that
    /// was true when they read it.
    async fn add_dependency(
        &self,
        blocked_task_id: &str,
        blocking_task_id: &str,
        at: OffsetDateTime,
    ) -> Result<()>;

    async fn clear_dependency(
        &self,
        blocked_task_id: &str,
        blocking_task_id: &str,
        at: OffsetDateTime,
    ) -> Result<()>;

    /// The author is the session's user, decided by the handler. There is no
    /// author field on the form.
    async fn add_comment(
        &self,
        task_id: &str,
        author_id: &str,
        body: &str,
        at: OffsetDateTime,
    ) -> Result<String>;

    /// Writes the title, description and deadline the detail screen saved, and
    /// records one activity line per field that actually changed.
    async fn save_task(
        &self,
        task_id: &str,
        title: &str,
        description: &str,
        deadline: Option<time::Date>,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<()>;

    /// Removes a task and every dependency edge it stood in. Tasks that were
    /// waiting only on this one become unblocked, and that is recorded in
    /// their activity — the rules engine will want to mail about it.
    ///
    /// Returns the ids of the tasks that were freed.
    async fn delete_task(
        &self,
        task_id: &str,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<Vec<String>>;

    /// Appends one line to a task's activity trail. `actor_id` is `None` when
    /// the system did it rather than a person.
    async fn record_activity(
        &self,
        task_id: &str,
        actor_id: Option<&str>,
        kind: &ActivityKind,
        detail: &str,
        at: OffsetDateTime,
    ) -> Result<()>;
}
