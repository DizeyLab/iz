//! Storage boundary.
//!
//! Everything the app does to persistent state goes through [`Store`]. The only
//! implementation today is Turso (in-process, SQLite-compatible); a Postgres
//! swap is a new impl of this trait and nothing else.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::Role;
use crate::board::{BoardReads, Moved, TaskRow};
use crate::detail::{ActivityKind, DeletionCost, DetailReads};

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

/// A workspace and the settings that ride on it. The sender is not here at
/// all: host, port, username, password and from-address come from the
/// environment, so no query can return them and no backup of this file carries
/// the password.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub created_at: OffsetDateTime,
    pub attachment_limit_bytes: u64,
    pub photo_limit_bytes: u64,
    pub allowed_file_types: Vec<String>,
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

/// What makes a rule fire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// A card crossed into this column. The crossing is the fact, not the
    /// card's current column: a card that goes Review -> Done -> Review has
    /// crossed into Done once.
    StatusBecomes(String),
    /// A task whose last blocker just finished, so the people on it can start.
    Unblocked,
}

/// Who a rule mails. A Viewer appears in neither list — a Viewer cannot be
/// assigned and is never mailed, and that is decided here rather than left to
/// whoever calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Audience {
    Assignees,
    Board,
}

/// One sentence: when this happens, send this subject to these people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailRule {
    pub id: String,
    pub board_id: String,
    pub trigger: Trigger,
    pub subject: String,
    pub audience: Audience,
    pub enabled: bool,
    pub created_at: OffsetDateTime,
}

/// Where one mail got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendState {
    /// Claimed by an engine run, not yet accepted by the mail server.
    Pending,
    Sent,
    /// The server refused in a way that may not be true later — a timeout, a
    /// host that is down. It will be tried again.
    Failed,
    /// Refused in a way that will not change, or out of attempts. Nobody will
    /// try again, and the admin can see that it was tried.
    Abandoned,
}

/// One mail a rule owes one person, and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSend {
    pub id: String,
    pub rule_id: String,
    /// The transition that caused it.
    pub event_id: String,
    pub task_id: String,
    pub recipient: String,
    pub state: SendState,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<OffsetDateTime>,
    pub sent_at: Option<OffsetDateTime>,
}

/// A deletion that freed something, kept so a mail owed because of it can be
/// rebuilt later. The task it names is gone by the time anyone reads this, so
/// its key and title are copied rather than looked up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Freeing {
    pub id: String,
    pub board_id: String,
    /// The deleted task, as the mail has to name it.
    pub cause_key: String,
    pub cause_title: String,
    pub actor_id: String,
    pub at: OffsetDateTime,
}

/// What a delete did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deletion {
    /// Tasks that were waiting only on the deleted one, and now wait on
    /// nothing.
    pub freed: Vec<String>,
    /// The recorded freeing, written only when something was actually freed:
    /// a delete that frees nobody is not an event any rule can fire on.
    pub event: Option<Freeing>,
}

/// The two things that can owe a mail. Both are committed facts with an id, an
/// actor and a moment, and a retry rebuilds its mail from whichever one the
/// ledger row points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// A card crossed into a column.
    Moved(crate::board::Transition),
    /// A blocker was deleted and let something go.
    Freed(Freeing),
}

impl Event {
    pub fn id(&self) -> &str {
        match self {
            Event::Moved(transition) => &transition.id,
            Event::Freed(freeing) => &freeing.id,
        }
    }

    /// Who did it. Nobody is ever mailed about their own action, so this is
    /// the address the engine takes off every audience.
    pub fn actor_id(&self) -> &str {
        match self {
            Event::Moved(transition) => &transition.actor_id,
            Event::Freed(freeing) => &freeing.actor_id,
        }
    }

    /// When it happened — the fact's own clock, not the sender's.
    pub fn at(&self) -> OffsetDateTime {
        match self {
            Event::Moved(transition) => transition.at,
            Event::Freed(freeing) => freeing.at,
        }
    }
}

/// Somebody a rule can mail. The address is here because this is the one place
/// that needs it; it never rides out to a page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipient {
    pub user_id: String,
    pub email: String,
    pub display_name: String,
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

    async fn set_limits(
        &self,
        workspace_id: &str,
        attachment_limit_bytes: u64,
        photo_limit_bytes: u64,
        allowed_file_types: &[String],
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

    /// Moves a task into a column and records the crossing, both in one
    /// transaction, so a transition never exists without the move that caused
    /// it and a move never happens without leaving the fact behind.
    ///
    /// `from_column_id` is the column the caller believed the card was in when
    /// the drag started, and the update is conditional on it still being true.
    /// Two people dragging the same card at once therefore produce exactly one
    /// transition: the loser is told [`Moved::Stale`] and re-reads, rather than
    /// writing a second crossing out of a column the card had already left.
    ///
    /// A card dropped back where it came from answers [`Moved::Unchanged`] and
    /// writes nothing at all — not the transition, not the activity line, not
    /// an `updated_at` bump. It did not move.
    ///
    /// Moving into a column with `is_done` set stamps `done_at`; moving out of
    /// one clears it, in the same transaction, because the card's finished
    /// state is a consequence of where it sits and must not be able to drift
    /// from it.
    async fn move_task(
        &self,
        task_id: &str,
        from_column_id: &str,
        to_column_id: &str,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<Moved>;

    /// Removes a task and every dependency edge it stood in. Tasks that were
    /// waiting only on this one become unblocked, that is recorded in their
    /// activity, and — when anything was freed — the freeing itself is written
    /// as an event the rules engine can fire on and a retry can read back.
    async fn delete_task(
        &self,
        task_id: &str,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<Deletion>;

    /// What a delete would take with it, for the confirmation step. Reads
    /// only; nothing here writes.
    async fn deletion_cost(&self, task_id: &str) -> Result<Option<DeletionCost>>;

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

    // -- mail rules --------------------------------------------------------

    async fn create_mail_rule(
        &self,
        board_id: &str,
        trigger: &Trigger,
        subject: &str,
        audience: Audience,
        at: OffsetDateTime,
    ) -> Result<MailRule>;

    /// Every rule on the board, switched off ones included: the admin screen
    /// lists what exists, not what is live.
    async fn mail_rules(&self, board_id: &str) -> Result<Vec<MailRule>>;

    /// One rule, for a retry that has only the send row to go on.
    async fn mail_rule(&self, rule_id: &str) -> Result<Option<MailRule>>;

    /// One event, by id, whether it was a crossing or a freeing. A retry
    /// rebuilds its mail from the facts as they were committed, and the
    /// event's own clock is one of them: a send retried on Thursday still says
    /// the card moved on Tuesday.
    async fn event(&self, event_id: &str) -> Result<Option<Event>>;

    async fn set_mail_rule_enabled(&self, rule_id: &str, enabled: bool) -> Result<()>;

    async fn delete_mail_rule(&self, rule_id: &str) -> Result<()>;

    /// When each rule last got a mail accepted, for the "last sent" line.
    async fn mail_rule_last_sent(&self, board_id: &str) -> Result<Vec<(String, OffsetDateTime)>>;

    // -- the send ledger ---------------------------------------------------

    /// Takes ownership of one mail by writing its row, and answers `None` if
    /// somebody already owns it.
    ///
    /// The unique index decides, not a preceding read: the engine running
    /// twice over one transition inserts twice and the second insert loses.
    /// Nothing is handed to the mail server before this row exists, so a
    /// crash mid-send leaves a row that says pending rather than a mail
    /// nobody can account for.
    async fn claim_send(
        &self,
        rule_id: &str,
        event_id: &str,
        task_id: &str,
        recipient: &str,
        at: OffsetDateTime,
    ) -> Result<Option<MailSend>>;

    /// The server took it.
    async fn record_send_accepted(&self, send_id: &str, at: OffsetDateTime) -> Result<()>;

    /// The server refused it. `retry_at` is `Some` while it is worth trying
    /// again and `None` when it never will be — a refused address, or the last
    /// attempt spent — and the answer the server gave is kept either way, so
    /// the admin can see that a rule tried and failed rather than wondering
    /// where the mail went.
    async fn record_send_refused(
        &self,
        send_id: &str,
        error: &str,
        retry_at: Option<OffsetDateTime>,
        at: OffsetDateTime,
    ) -> Result<()>;

    /// Sends owed right now: claimed but never accepted, and due.
    async fn sends_owed(&self, now: OffsetDateTime, limit: u32) -> Result<Vec<MailSend>>;

    /// Every send a rule has made, newest first, for the admin's trail.
    async fn sends_for_rule(&self, rule_id: &str, limit: u32) -> Result<Vec<MailSend>>;

    // -- who gets mailed ---------------------------------------------------

    /// The people a task points at. Viewers cannot be assigned, so none appear.
    async fn recipients_for_task(&self, task_id: &str) -> Result<Vec<Recipient>>;

    /// Everyone who may write on the board. Viewers are left out here, in the
    /// store, so no caller can mail one by forgetting to filter.
    async fn recipients_for_board(&self, board_id: &str) -> Result<Vec<Recipient>>;
}
