//! Storage boundary.
//!
//! Everything the app does to persistent state goes through [`Store`]. The only
//! implementation today is Turso (in-process, SQLite-compatible); a Postgres
//! swap is a new impl of this trait and nothing else.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::Role;
use crate::board::{BoardReads, Moved, TaskRow, Transition};
use crate::detail::{ActivityKind, DeletionCost, DetailReads};

#[cfg(feature = "server")]
pub mod secret;
#[cfg(feature = "server")]
pub mod reconcile;
#[cfg(feature = "server")]
pub mod schema;
pub mod turso_store;

pub use turso_store::TursoStore;
#[cfg(feature = "server")]
pub use reconcile::{reconcile, ReconcileOptions};

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
    /// Subtasks are one level deep. Either the would-be parent is itself a
    /// subtask, or the task being parented already has subtasks of its own.
    #[error("subtasks go one level deep")]
    NotNestable,
    /// A parent and its subtask live on the same board.
    #[error("that task is on another board")]
    OtherBoard,
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A workspace and the settings that ride on it.
///
/// The sender is here except for its password, which has no field on purpose:
/// this record is what handlers load and what pages serialise, so a password
/// with a field here is a password one careless response away from the wire.
/// It is written by [`Store::set_sender`] and read by [`Store::smtp_password`],
/// which only the mailer calls. What a screen gets instead is
/// `smtp_password_set` — enough to say "set" and nothing more.
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
    /// Whether a password is stored, never the password. Derived in the query
    /// so that the value itself does not travel even this far.
    pub smtp_password_set: bool,
    /// How the last "send test mail to myself" went, if one has been pressed
    /// since the sender was last edited.
    pub sender_test: Option<SenderTest>,
    /// How the last handshake with the mail server went, if one has happened
    /// since the sender was last edited. See [`SenderCheck`] on why this is
    /// not the same thing as `sender_test`.
    pub sender_check: Option<SenderCheck>,
    pub attachment_limit_bytes: u64,
    pub photo_limit_bytes: u64,
    pub allowed_file_types: Vec<String>,
    /// The quiet window a notification waits out before it is sent, in
    /// minutes. `0` sends every trigger the moment it is owed.
    pub mail_batch_minutes: u32,
    /// How long before a task's clock its reminder mail is queued, in
    /// minutes. `0` turns reminders off.
    pub reminder_minutes: u32,
    /// The origin mail links point at, when an admin has set one. `None`
    /// means the address the process listens on — a box behind a proxy
    /// answers on localhost and is reached on a public name, and only an
    /// admin knows which.
    pub public_url: Option<String>,
}

/// One file hung off a task, as a screen lists it: a name, a type, a size and
/// who put it there. The bytes are deliberately not on this type — it is what
/// handlers load and pages serialise, and a file's contents have no business
/// travelling with a list of file names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub task_id: String,
    /// The comment it was posted with, when it was posted with one.
    pub comment_id: Option<String>,
    /// What the browser called the file. A label, never a path.
    pub file_name: String,
    /// What the server decided the bytes are, never what the upload claimed.
    pub mime_type: String,
    pub size_bytes: u64,
    pub uploaded_by: String,
    pub uploaded_at: OffsetDateTime,
}

/// A file on its way into the table, bytes and all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment<'a> {
    pub task_id: &'a str,
    pub comment_id: Option<&'a str>,
    pub file_name: &'a str,
    pub mime_type: &'a str,
    pub bytes: Vec<u8>,
    pub uploaded_by: &'a str,
    pub at: OffsetDateTime,
}

/// A sender as an admin typed it, on its way to the table.
///
/// `password` is `None` when the admin left the field untouched, which is what
/// the screen sends when a password is already stored: the field is write-only,
/// so an edit to the port must not blank the password just because the form had
/// nothing to put in it.
/// What came of pressing the test button: when, how long the mail server took,
/// and what it said if it refused.
///
/// `error` is `None` for a mail that was accepted. It holds the mail server's
/// own words otherwise, because "it did not work" is not something an admin can
/// act on and "535 authentication failed" is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderTest {
    pub at: OffsetDateTime,
    pub took_ms: u64,
    pub error: Option<String>,
}

/// How the last handshake with the mail server went.
///
/// Not the same fact as [`SenderTest`], and kept apart on purpose. A test sends
/// a real message to a real inbox and proves delivery. A check connects,
/// negotiates TLS, says hello, authenticates and hangs up — it proves the host,
/// the port, the encryption and the password without sending anything to
/// anybody. What it cannot prove is that the from-address is one this account
/// is allowed to send as, which is the usual reason a login that works is
/// followed by mail that bounces.
///
/// `error` is `None` when the server let us in, and otherwise holds what the
/// server said, because "535 authentication failed" is actionable and "it did
/// not work" is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderCheck {
    pub at: OffsetDateTime,
    pub took_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewSender {
    pub host: String,
    pub port: u32,
    pub username: String,
    pub password: Option<String>,
    pub from_name: String,
    pub from_address: String,
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
    pub has_photo: bool,
    pub created_at: OffsetDateTime,
    pub last_signed_in_at: Option<OffsetDateTime>,
    /// Who made this account. Null for the first account, which nobody
    /// invited.
    pub invited_by: Option<String>,
    /// Display-only. Stored data stays UTC/neutral; this only changes how a
    /// browser renders it for this one person.
    pub timezone: String,
    /// Display-only, as [`Self::timezone`].
    pub theme: String,
    /// Display-only, as [`Self::timezone`].
    pub language: String,
    /// Display-only, as [`Self::timezone`].
    pub ui: String,
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
    /// The admin who is making this account, or none for the first one.
    pub invited_by: Option<String>,
}

/// The profile page's totals for one person: what is on their plate, what
/// finished under them, what they opened, what they said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserStats {
    /// Tasks the person is on now, sitting in a column that is not done.
    pub assigned_open: u32,
    /// Tasks they are on that reached a done column.
    pub assigned_done: u32,
    /// Tasks they opened, whoever carries them now.
    pub created: u32,
    /// Comments they wrote.
    pub comments: u32,
}

/// What a link is for. The same table and the same token machinery carry
/// both; the kind decides which redeeming flow may spend it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// An invited person's first sign-in.
    Join,
    /// A self-serve password reset, mailed to the account's own address.
    Reset,
}

impl LinkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkKind::Join => "join",
            LinkKind::Reset => "reset",
        }
    }

    pub fn from_str(raw: &str) -> Option<LinkKind> {
        match raw {
            "join" => Some(LinkKind::Join),
            "reset" => Some(LinkKind::Reset),
            _ => None,
        }
    }
}

/// A mailed link — an invitation or a password reset. Only the hash of the
/// token is ever stored; the plaintext is shown once, when the link is
/// created or mailed.
#[derive(Debug, Clone, PartialEq)]
pub struct SigninLink {
    pub id: String,
    pub user_id: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub used_at: Option<OffsetDateTime>,
    pub kind: LinkKind,
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
    /// The task this one is a subtask of, if any. It must be on the same
    /// board and must not itself be a subtask.
    pub parent_id: Option<&'a str>,
    pub title: &'a str,
    pub description: &'a str,
    pub deadline: Option<time::Date>,
    /// The meeting instant, an exact time — `None` for a task without one.
    /// A clock set at birth queues its reminders with the task itself.
    pub clock_at: Option<OffsetDateTime>,
    pub created_by: &'a str,
}

/// What `create_task` wrote: the task, the id of the Created activity row
/// it filed alongside it, so a caller that needs to name the event — a mail
/// retry, one day — does not have to go read the trail back, and the
/// transition into its starting column, so a rule watching that column
/// fires the same as it would for a card dropped into it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCreated {
    pub row: TaskRow,
    pub activity_id: String,
    pub transition: Transition,
}

/// What `add_comment` wrote: the comment's id, and the id of the Commented
/// activity row it filed alongside it in the same transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentWritten {
    pub comment_id: String,
    pub activity_id: String,
}

/// What makes a rule fire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// A card crossed into this column. The crossing is the fact, not the
    /// card's current column: a card that goes Review -> Done -> Review has
    /// crossed into Done once. `None` watches every column — any crossing at
    /// all fires it, which is how one rule covers a whole board.
    StatusBecomes(Option<String>),
    /// A task whose last blocker just finished, so the people on it can start.
    Unblocked,
    Created,
    Assigned,
    Unassigned,
    Commented,
    DeadlineSet,
    DeadlineCleared,
    Retitled,
    Linked,
    Unlinked,
    Deleted,
}

/// Who a rule mails. A Viewer appears in neither list — a Viewer cannot be
/// assigned and is never mailed, and that is decided here rather than left to
/// whoever calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Audience {
    Assignees,
    Board,
    /// Whoever opened the card.
    Creator,
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
    /// Fold the task's own facts — key, title, column, deadline, assignees —
    /// into the mail body, instead of the body being only the rule's static
    /// sentence.
    pub include_task_details: bool,
}

/// A tag is the project a task belongs to. A task wears at most one, so the
/// link is a column on the task, not a join table. Tags belong to a board,
/// like the mail rules do, and their order is the admin's — set by hand, so
/// it is stored rather than derived. One tag per board is the default its
/// tasks fall back to, and it cannot be deleted — only renamed and
/// reordered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: String,
    pub board_id: String,
    pub name: String,
    pub position: i64,
    pub is_default: bool,
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

/// Which shape a [`MailSend`] row is: a rule's mail about a task, an invite
/// that owes no rule, no event and no task, or a reminder minted straight
/// onto the clock it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendKind {
    Rule,
    Invite,
    /// An admin's own mail to one or more members — no rule, no event, no
    /// task, same as an invite.
    Notice,
    /// The "your meeting starts soon" mail a task's clock owes each assignee.
    /// Like an invite it carries its own subject and body, but it hangs off a
    /// task and falls due in the future, so the queue delivers it when the
    /// reminder window opens, not when it is written.
    Reminder,
}

impl SendKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SendKind::Rule => "rule",
            SendKind::Invite => "invite",
            SendKind::Notice => "notice",
            SendKind::Reminder => "reminder",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "rule" => Some(SendKind::Rule),
            "invite" => Some(SendKind::Invite),
            "notice" => Some(SendKind::Notice),
            "reminder" => Some(SendKind::Reminder),
            _ => None,
        }
    }
}

/// A send this delivery pass holds, and the only thing the engine will mail.
///
/// The duplicate-invite bug was not that somebody forgot to claim a row. It
/// was that the delivery path *could* read a row and mail it without claiming
/// it — two passes did, and an invited member got two identical mails. A
/// comment saying "claim first" is a fix that lasts until the next person
/// writes the next delivery loop.
///
/// So the reading and the claiming are one operation
/// ([`claim_sends_owed`](Store::claim_sends_owed)), and what it hands back is
/// this rather than a bare [`MailSend`]. Only the store builds one, and
/// `Engine::attempt` accepts nothing else, so a row that was merely *read* —
/// by [`sends_owed`](Store::sends_owed), by a queue screen, by a test — will
/// not compile into a mail. The rule is carried by the type instead of by
/// whoever remembers it.
#[derive(Debug, Clone)]
pub struct ClaimedSend(MailSend);

impl ClaimedSend {
    /// Built only where a row was actually taken: the store's claim.
    pub(crate) fn taken(send: MailSend) -> Self {
        Self(send)
    }
}

impl std::ops::Deref for ClaimedSend {
    type Target = MailSend;

    fn deref(&self) -> &MailSend {
        &self.0
    }
}

/// One mail a rule owes one person, or an invite that owes nobody, and what
/// happened to it. `rule_id`/`event_id`/`task_id` are `None` on an invite
/// send; `subject`/`body` are `None` on a rule send, which composes its own
/// from the rule and the event instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSend {
    pub id: String,
    pub rule_id: Option<String>,
    /// The transition that caused it.
    pub event_id: Option<String>,
    pub task_id: Option<String>,
    pub recipient: String,
    pub state: SendState,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<OffsetDateTime>,
    pub sent_at: Option<OffsetDateTime>,
    pub kind: SendKind,
    pub subject: Option<String>,
    pub body: Option<String>,
}

/// What a rule decided about one event, win or not. Written for every rule an
/// event touches, not only the ones that owed a mail, so "why did nobody get
/// mailed" has a row to read instead of a log to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailOutcome {
    /// A send was claimed for this.
    Owed,
    /// A send already existed; this is a replay.
    AlreadyOwed,
    /// The audience resolved to nobody.
    NoRecipients,
    /// The event did not fire this rule.
    NotMatched,
    /// The rule was off when the event happened.
    Disabled,
    /// The task named by the event is deleted.
    TaskGone,
}

impl MailOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            MailOutcome::Owed => "owed",
            MailOutcome::AlreadyOwed => "already_owed",
            MailOutcome::NoRecipients => "no_recipients",
            MailOutcome::NotMatched => "not_matched",
            MailOutcome::Disabled => "disabled",
            MailOutcome::TaskGone => "task_gone",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "owed" => Some(MailOutcome::Owed),
            "already_owed" => Some(MailOutcome::AlreadyOwed),
            "no_recipients" => Some(MailOutcome::NoRecipients),
            "not_matched" => Some(MailOutcome::NotMatched),
            "disabled" => Some(MailOutcome::Disabled),
            "task_gone" => Some(MailOutcome::TaskGone),
            _ => None,
        }
    }
}

/// One row of what a rule decided about one event and task, whether or not it
/// owed a mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailDecision {
    pub id: String,
    pub rule_id: String,
    pub event_id: String,
    pub task_id: String,
    pub outcome: MailOutcome,
    /// Empty when the outcome says everything, otherwise a machine token
    /// (`empty`, `not_status`, `moved:<to_col_id>:<watch_col_id>`, ...) that
    /// `logs.rs` renders in the viewer's language; column references are IDs,
    /// never names, so a renamed or deleted column does not strand English in
    /// the ledger. Rows written before this scheme carry English prose
    /// instead and are shown as written — history does not get rewritten.
    pub detail: String,
    pub at: OffsetDateTime,
}

/// One line of the workspace-wide activity feed: what happened, on which
/// task when there was one, and who did it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityLine {
    pub id: String,
    /// Absent when the line is an account or admin event, which has no task.
    pub task_id: Option<String>,
    pub title: Option<String>,
    /// The task's own key (e.g. `PROJ-12`), absent alongside `task_id`.
    pub task_key: Option<String>,
    /// Absent when the system did it rather than a person.
    pub actor_name: Option<String>,
    pub kind: ActivityKind,
    pub detail: String,
    pub at: OffsetDateTime,
}

/// A keyset position in one of the logs feeds: the moment and the row's own
/// id, the same pair the feed orders by, so paging never relies on a row
/// count that can shift under a writer between page turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedCursor {
    pub at: OffsetDateTime,
    pub id: String,
}

/// Which slice of a feed to read: the newest rows, strictly older than a
/// cursor (an "Older" turn), or strictly newer (a "Newer" turn back toward
/// the top). `Before`/`After` name the cursor's position relative to the
/// page returned, not the feed's own sort direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeedPage {
    Newest,
    Before(FeedCursor),
    After(FeedCursor),
}

/// Which end of the activity feed reading order starts at. `Before` always
/// means further along in this order (an "Older" turn from `Newest`, a
/// "toward the end" turn from `Oldest`); `After` always means back toward
/// the start — the two feel symmetric to the reader regardless of which
/// direction is currently in play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Newest,
    Oldest,
}

/// The activity tab's narrowing: every field is an AND'd equality (or a
/// half-open range for `day`), and an absent field matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityFilter {
    /// A user id, or the literal `"system"` for a row with no actor.
    pub actor: Option<String>,
    /// An `ActivityKind::as_str()` value.
    pub kind: Option<String>,
    pub task_key: Option<String>,
    /// Half-open `[start, end)` in UTC, already resolved from the admin's
    /// timezone.
    pub day: Option<(OffsetDateTime, OffsetDateTime)>,
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
    /// The id of the Deleted activity row, always written, so the mail engine
    /// can fire on it the same way it does the rest of a task's trail.
    pub activity_id: String,
}

/// One row of a task's activity trail, as an event a rule can fire on. Covers
/// everything `record_activity` writes that is not already its own event
/// shape — a comment today, whatever else `ActivityKind` grows later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: String,
    pub task_id: String,
    pub board_id: String,
    pub kind: ActivityKind,
    pub actor_id: String,
    /// The person the line is about — the one just assigned, say — when it
    /// is about anyone in particular.
    pub subject_id: Option<String>,
    pub detail: String,
    pub at: OffsetDateTime,
}

/// The things that can owe a mail. Each is a committed fact with an id, an
/// actor and a moment, and a retry rebuilds its mail from whichever one the
/// ledger row points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// A card crossed into a column.
    Moved(crate::board::Transition),
    /// A blocker was deleted and let something go.
    Freed(Freeing),
    /// An activity row that is neither of the above — a comment, today.
    Happened(ActivityEvent),
}

impl Event {
    pub fn id(&self) -> &str {
        match self {
            Event::Moved(transition) => &transition.id,
            Event::Freed(freeing) => &freeing.id,
            Event::Happened(activity) => &activity.id,
        }
    }

    /// Who did it. Nobody is ever mailed about their own action, so this is
    /// the address the engine takes off every audience.
    pub fn actor_id(&self) -> &str {
        match self {
            Event::Moved(transition) => &transition.actor_id,
            Event::Freed(freeing) => &freeing.actor_id,
            Event::Happened(activity) => &activity.actor_id,
        }
    }

    /// When it happened — the fact's own clock, not the sender's.
    pub fn at(&self) -> OffsetDateTime {
        match self {
            Event::Moved(transition) => transition.at,
            Event::Freed(freeing) => freeing.at,
            Event::Happened(activity) => activity.at,
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
    // -- live updates ------------------------------------------------------

    /// A receiver of committed-write announcements. One [`Change`] per
    /// write that committed, carrying the topic to re-fetch and no data:
    /// the channel cannot say more than the reader may hear, because it
    /// says nothing — the woken client re-fetches through the ordinary
    /// role-gated route. Sending to zero subscribers is normal and silent,
    /// and a slow subscriber's overflow is the client's cue to resync.
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::live::Change>;
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

    /// Writes the sender. A `password` of `None` leaves the stored one alone,
    /// which is how an admin changes the port without retyping a secret the
    /// screen was never allowed to show them.
    async fn set_sender(&self, workspace_id: &str, sender: NewSender) -> Result<()>;

    /// Writes down how the last test send went. Editing the sender clears it,
    /// so what is stored is always about the settings that are stored.
    async fn record_sender_test(&self, workspace_id: &str, test: SenderTest) -> Result<()>;

    /// Writes down how the last handshake went. Separate from
    /// [`Store::record_sender_test`] so a login that worked can never be
    /// rendered as a mail that arrived.
    async fn record_sender_check(&self, workspace_id: &str, check: SenderCheck) -> Result<()>;

    /// Reads the sender's password. Only the mailer calls this, and nothing it
    /// returns reaches a response body.
    async fn smtp_password(&self, workspace_id: &str) -> Result<Option<String>>;

    /// Writes the address mail links point at. `None` clears it, and the
    /// address the process listens on is what a cleared one falls back to.
    async fn set_public_url(&self, workspace_id: &str, public_url: Option<&str>) -> Result<()>;

    /// Writes every workspace knob the settings page saves in one form: the
    /// two attachment ceilings, the allowed file types, the notification
    /// quiet window and the reminder lead. One write, because they are one
    /// save — a second way to write the same row would let the knobs
    /// disagree about what the admin last chose.
    async fn set_limits(
        &self,
        workspace_id: &str,
        attachment_limit_bytes: u64,
        photo_limit_bytes: u64,
        allowed_file_types: &[String],
        mail_batch_minutes: u32,
        reminder_minutes: u32,
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

    /// One person's totals, for their profile. A soft-deleted task counts
    /// nowhere: it has left the board, so it has left the person.
    ///
    /// This is a read, for looking: nothing announces.
    async fn user_stats(&self, user_id: &str) -> Result<UserStats>;

    async fn set_password_hash(&self, user_id: &str, hash: &str) -> Result<()>;

    async fn set_profile(&self, user_id: &str, display_name: &str) -> Result<()>;

    /// Stores the profile photo's bytes and mime type, replacing any previous one.
    async fn set_photo(&self, user_id: &str, bytes: &[u8], mime: &str) -> Result<()>;

    async fn clear_photo(&self, user_id: &str) -> Result<()>;

    /// The photo's bytes and mime type, or `None` when none is set.
    async fn photo(&self, user_id: &str) -> Result<Option<(Vec<u8>, String)>>;

    /// Changes the sign-in address. Refuses with [`StoreError::Conflict`] if
    /// another account in the same workspace already holds it — the same
    /// wording `create_user` refuses a duplicate invite with.
    async fn set_email(&self, user_id: &str, workspace_id: &str, email: &str) -> Result<()>;

    /// Display-only preferences: stored data stays UTC/neutral, these only
    /// change how a browser renders it for this one person.
    async fn set_preferences(
        &self,
        user_id: &str,
        timezone: &str,
        theme: &str,
        language: &str,
        ui: &str,
    ) -> Result<()>;

    async fn set_role(&self, user_id: &str, role: Role) -> Result<()>;

    async fn mark_signed_in(&self, user_id: &str, at: OffsetDateTime) -> Result<()>;

    // -- sign-in links -----------------------------------------------------

    /// Stores the hash of a freshly minted link. The caller keeps the plaintext
    /// and shows it once. The kind decides which redeeming flow may spend it.
    async fn create_signin_link(
        &self,
        user_id: &str,
        token_hash: &str,
        expires_at: OffsetDateTime,
        kind: LinkKind,
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
    async fn create_task(&self, new: NewTask<'_>) -> Result<TaskCreated>;

    /// Idempotent: assigning someone twice is not an error.
    /// Makes `task_id` a subtask of `parent_id`, or `None` to promote it back
    /// to a task of its own. Both directions are one write: that is the whole
    /// reason a subtask is a task with a parent rather than a row in a table
    /// of its own.
    ///
    /// Refuses with [`StoreError::NotNestable`] when the move would make two
    /// levels — the parent is already somebody's subtask, or the task being
    /// parented has subtasks of its own — and with [`StoreError::OtherBoard`]
    /// when the two are not on the same board.
    async fn set_parent(&self, task_id: &str, parent_id: Option<&str>) -> Result<()>;

    /// The live subtasks of `parent_id`, oldest first. Empty for a task that
    /// has none, and for a subtask, which cannot have any.
    async fn subtasks(&self, parent_id: &str) -> Result<Vec<TaskRow>>;

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
    /// author field on the form. Writes the comment and its Commented
    /// activity row in the same transaction, so one never exists without the
    /// other.
    async fn add_comment(
        &self,
        task_id: &str,
        author_id: &str,
        body: &str,
        at: OffsetDateTime,
    ) -> Result<CommentWritten>;

    /// Hangs a file off a task. The bytes go into the database file with the
    /// row: there is no second place for an İzlek deployment to keep, and no
    /// path for an uploaded name to become.
    ///
    /// Nothing here decides whether the file was allowed — its size, its type
    /// and who may attach it are the handler's questions, answered before the
    /// bytes reach this call.
    async fn add_attachment(&self, new: NewAttachment<'_>) -> Result<String>;

    /// What is hung off a task, oldest first, without the bytes. This is what
    /// a screen reads; a screen never carries a file's contents.
    async fn attachments(&self, task_id: &str) -> Result<Vec<Attachment>>;

    /// One file's row, still without its bytes. The `task_id` on it is what the
    /// handler checks before it hands anything over.
    async fn attachment(&self, id: &str) -> Result<Option<Attachment>>;

    /// The bytes themselves, for the one handler that serves them.
    async fn attachment_bytes(&self, id: &str) -> Result<Option<Vec<u8>>>;

    /// Takes a file away for good. `false` when there was no such row.
    async fn delete_attachment(&self, id: &str) -> Result<bool>;

    /// Writes the title, description, deadline and clock the detail screen
    /// saved, and records one activity line per field that actually changed.
    /// Returns the ids of the activity rows it wrote, in write order — empty
    /// when nothing changed — so the caller can hand each to the mail engine.
    async fn save_task(
        &self,
        task_id: &str,
        title: &str,
        description: &str,
        deadline: Option<time::Date>,
        clock_at: Option<OffsetDateTime>,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<Vec<String>>;

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

    /// Appends one line to a task's activity trail and returns its row id.
    /// `actor_id` is `None` when the system did it rather than a person.
    /// `subject_id` is the person the line is ABOUT — the one just assigned,
    /// say — and `None` for a line that is about nobody.
    async fn record_activity(
        &self,
        task_id: &str,
        actor_id: Option<&str>,
        subject_id: Option<&str>,
        kind: &ActivityKind,
        detail: &str,
        at: OffsetDateTime,
    ) -> Result<String>;

    /// Appends one line to the workspace-wide event trail — an account or
    /// admin moment with no task to live under — and returns its row id.
    async fn record_event(
        &self,
        actor_id: Option<&str>,
        kind: &ActivityKind,
        detail: &str,
        at: OffsetDateTime,
    ) -> Result<String>;

    // -- mail rules --------------------------------------------------------

    async fn create_mail_rule(
        &self,
        board_id: &str,
        trigger: &Trigger,
        subject: &str,
        audience: Audience,
        at: OffsetDateTime,
        include_task_details: bool,
    ) -> Result<MailRule>;

    /// Every rule on the board, switched off ones included: the admin screen
    /// lists what exists, not what is live.
    async fn mail_rules(&self, board_id: &str) -> Result<Vec<MailRule>>;

    /// Rewrites a rule's sentence — its trigger, subject and audience — in
    /// place. `enabled` and `created_at` are untouched, so an edit does not
    /// silently turn a rule back on or reset when it was made.
    async fn update_mail_rule(
        &self,
        rule_id: &str,
        trigger: &Trigger,
        subject: &str,
        audience: Audience,
        include_task_details: bool,
    ) -> Result<()>;

    /// The board a task belongs to, reading through the soft delete —
    /// `None` only if the task id never existed. A crossing whose task was
    /// deleted before the engine ran still owes its rules a `task_gone` row,
    /// and this is how the engine finds which board's rules those are.
    async fn board_of_task(&self, task_id: &str) -> Result<Option<String>>;

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

    // -- tags --------------------------------------------------------------

    /// Every tag on the board, in the admin's hand-set order.
    async fn tags(&self, board_id: &str) -> Result<Vec<Tag>>;

    /// How many live cards wear each of the board's tags, keyed by tag id. A
    /// tag nobody uses is absent rather than zero — the caller reads a
    /// missing key as none, which is what a fresh tag is.
    async fn tag_task_counts(&self, board_id: &str) -> Result<Vec<(String, u32)>>;

    /// Appends a tag at the end of the board's order. The name is unique per
    /// board: two tags with one name are one project spelled twice.
    async fn create_tag(&self, board_id: &str, name: &str, at: OffsetDateTime) -> Result<Tag>;

    async fn rename_tag(&self, tag_id: &str, name: &str) -> Result<()>;

    /// Deletes a tag. Its tasks move to the board's default tag; the default
    /// itself is refused — it is where they would go.
    /// Retires a tag. A tag with cards on it is refused with
    /// [`StoreError::Conflict`] — the cards are the reason it exists, and
    /// moving somebody's whole project onto the default because one menu item
    /// was clicked is not a deletion the admin asked for. The default tag is
    /// refused as well: it is where a card with no project of its own lives.
    async fn delete_tag(&self, tag_id: &str) -> Result<()>;

    /// Swaps a tag with its neighbour in the order. A tag already at that end
    /// stays put: nothing to swap is not an error.
    async fn move_tag(&self, tag_id: &str, up: bool) -> Result<()>;

    /// Sets the tag a task wears. A tag from another board is
    /// [`StoreError::NotFound`] — it is not one of this board's projects at
    /// all.
    async fn set_task_tag(&self, task_id: &str, tag_id: &str) -> Result<()>;
    // -- the send ledger ---------------------------------------------------

    /// Takes ownership of one mail by writing its row, and answers `None` if
    /// somebody already owns it.
    ///
    /// The unique index decides, not a preceding read: the engine running
    /// twice over one transition inserts twice and the second insert loses.
    /// Nothing is handed to the mail server before this row exists, so a
    /// crash mid-send leaves a row that says pending rather than a mail
    /// nobody can account for.
    ///
    /// The row is born held until `until`, because writing it announces the
    /// queue and the sweep wakes on that announcement — a row inserted due
    /// now is a row the sweep can be mailing while the pass that created it
    /// is also mailing it. The index stops the crossing being owed twice; the
    /// lease stops the one row being sent twice.
    async fn claim_send(
        &self,
        rule_id: &str,
        event_id: &str,
        task_id: &str,
        recipient: &str,
        at: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Result<Option<ClaimedSend>>;

    /// Holds an invite mail: pending, no rule, no event, no task.
    async fn queue_invite(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
        at: OffsetDateTime,
    ) -> Result<MailSend>;

    /// Holds an admin's mail to one member: pending, no rule, no event, no
    /// task, same shape as [`Store::queue_invite`].
    async fn queue_notice(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
        at: OffsetDateTime,
    ) -> Result<MailSend>;

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

    /// Holds a send that was never attempted, because the workspace has no
    /// sender. The reason is written down like any refusal so the ledger does
    /// not go quiet, but `attempts` is left alone: nothing was spent, so
    /// nothing is charged, and the mail is still owed when a sender appears.
    async fn defer_send(
        &self,
        send_id: &str,
        reason: &str,
        retry_at: OffsetDateTime,
        at: OffsetDateTime,
    ) -> Result<()>;

    /// Sends owed right now: claimed but never accepted, and due.
    ///
    /// This is a read, for looking: what it returns is a [`MailSend`], which
    /// the engine cannot mail. Delivery goes through
    /// [`claim_sends_owed`](Store::claim_sends_owed).
    async fn sends_owed(&self, now: OffsetDateTime, limit: u32) -> Result<Vec<MailSend>>;

    /// Takes the sends owed right now for this pass, and hands back only the
    /// ones it actually got.
    ///
    /// Two things deliver: the pass spawned off the request that queued the
    /// mail, and the sweep, which the same queue write wakes. Both used to
    /// read the ledger and then go to the mail server, and a row does not stop
    /// looking owed until the send comes back — so both saw the same row as
    /// owed and an invited member was mailed twice.
    ///
    /// Reading and claiming are therefore one operation. Every row returned
    /// has already been moved out of the due window, in the same call, before
    /// anyone can spend a network round-trip on it; a pass arriving second
    /// gets a shorter list, not a duplicate.
    ///
    /// `until` is a lease rather than a removal, because a process that dies
    /// between claiming and recording must not take the mail with it. The row
    /// falls due again then, and is sent late rather than never — which is the
    /// right way round for a mail nobody has received.
    async fn claim_sends_owed(
        &self,
        now: OffsetDateTime,
        until: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<ClaimedSend>>;

    /// When the next mail falls due, whether or not that is yet.
    ///
    /// The sweep asks this so it can sleep exactly that long instead of waking
    /// on a fixed beat. A beat is what makes a retry promised for 16:42:47 go
    /// out at 16:43: the row was due, and nothing was awake to notice.
    async fn next_due_at(&self) -> Result<Option<OffsetDateTime>>;

    /// Every send a rule has made, newest first, for the admin's trail.
    async fn sends_for_rule(&self, rule_id: &str, limit: u32) -> Result<Vec<MailSend>>;

    /// Every send this task's events have caused, newest first, for the task
    /// detail's notifications block.
    async fn sends_for_task(&self, task_id: &str, limit: u32) -> Result<Vec<MailSend>>;

    /// Puts a failed or abandoned send back in play: pending, due at `at`.
    /// `attempts` is left alone — this is a new try, not a rewrite of the
    /// ones already spent. A send that already went out is untouched.
    async fn requeue_send(&self, send_id: &str, at: OffsetDateTime) -> Result<()>;

    // -- mail decisions and observability -----------------------------------

    /// Records what a rule decided about one event and task. Idempotent: a
    /// replayed event lands on the row that is already there rather than
    /// beside it.
    #[allow(clippy::too_many_arguments)]
    async fn record_mail_decision(
        &self,
        rule_id: &str,
        event_id: &str,
        task_id: &str,
        outcome: MailOutcome,
        detail: &str,
        at: OffsetDateTime,
    ) -> Result<()>;

    /// The most recent decisions across every rule, newest first.
    async fn recent_mail_decisions(&self, limit: u32, page: FeedPage) -> Result<Vec<MailDecision>>;

    /// Every decision written about one task, newest first, for its
    /// notifications block.
    async fn decisions_for_task(&self, task_id: &str, limit: u32) -> Result<Vec<MailDecision>>;

    /// When each rule last decided anything, for the "last checked" line.
    async fn mail_rule_last_decision(&self) -> Result<Vec<(String, OffsetDateTime)>>;

    /// What is still owed: claimed but not yet accepted, and anything that
    /// failed and may be tried again.
    async fn mail_queue(&self, limit: u32, page: FeedPage) -> Result<Vec<MailSend>>;

    /// How many rows `mail_queue` draws from, unpaged — for the tab's
    /// position note.
    async fn count_mail_queue(&self) -> Result<u64>;

    /// How many queue rows sit strictly ahead of `cursor` in the queue's own
    /// order (soonest first); `None` (the newest page) is `0`.
    async fn count_mail_queue_preceding(&self, cursor: Option<&FeedCursor>) -> Result<u64>;

    /// Every send, whatever its state, newest first.
    async fn recent_sends(&self, limit: u32) -> Result<Vec<MailSend>>;

    /// How many decisions there are in all, unpaged.
    async fn count_mail_decisions(&self) -> Result<u64>;

    /// How many decision rows sit strictly ahead of `cursor` in the
    /// decisions' own order (newest first); `None` is `0`.
    async fn count_mail_decisions_preceding(&self, cursor: Option<&FeedCursor>) -> Result<u64>;

    /// The workspace's activity feed across every task, matching `filter`,
    /// ordered per `dir`.
    async fn recent_activity(
        &self,
        limit: u32,
        page: FeedPage,
        dir: Dir,
        filter: &ActivityFilter,
    ) -> Result<Vec<ActivityLine>>;

    /// How many activity rows match `filter`, unpaged.
    async fn count_activity(&self, filter: &ActivityFilter) -> Result<u64>;

    /// Every live task's key and title, key-ordered — the activity feed's
    /// task filter.
    async fn task_directory(&self) -> Result<Vec<(String, String)>>;

    /// How many rows matching `filter` sit strictly ahead of `cursor` in
    /// `dir`'s reading order; `None` (the newest/oldest page) is `0`.
    async fn count_activity_preceding(
        &self,
        filter: &ActivityFilter,
        dir: Dir,
        cursor: Option<&FeedCursor>,
    ) -> Result<u64>;

    /// Puts `user_id` on a task's watchers. Idempotent: watching twice is
    /// watching once.
    async fn watch_task(&self, task_id: &str, user_id: &str) -> Result<()>;

    /// Takes `user_id` off a task's watchers. Absent watch, absent row: no
    /// error.
    async fn unwatch_task(&self, task_id: &str, user_id: &str) -> Result<()>;

    /// The "what changed for me" feed: activity events on the tasks the user
    /// watches, plus events that name them (`subject_id` — an assignment
    /// outlives the watch it created), minus the user's own actions. Newest
    /// first, capped at `limit`.
    async fn feed_for_user(&self, user_id: &str, limit: u32) -> Result<Vec<ActivityLine>>;

    /// How many feed lines have landed since the user last read the feed.
    async fn count_feed_unseen(&self, user_id: &str) -> Result<u64>;

    /// Reads the feed to `at`: the unseen count resets, the history stays.
    async fn mark_feed_seen(&self, user_id: &str, at: OffsetDateTime) -> Result<()>;

    // -- who gets mailed ---------------------------------------------------

    /// The people a task points at. Viewers cannot be assigned, so none appear.
    async fn recipients_for_task(&self, task_id: &str) -> Result<Vec<Recipient>>;

    /// Everyone who may write on the board. Viewers are left out here, in the
    /// store, so no caller can mail one by forgetting to filter.
    async fn recipients_for_board(&self, board_id: &str) -> Result<Vec<Recipient>>;

    /// Pushes every mail still merely owed about this task, for this person,
    /// out to `until` — but never past `cap`, the moment the oldest of them
    /// stops waiting no matter what.
    ///
    /// This is the debounce: each new trigger about a card postpones the mail
    /// the card already owes, so a person is told once, after the work has
    /// settled. Only rows that are still pending and have never been
    /// attempted move — a mail already refused is on its own retry clock, and
    /// a batch must not drag it back.
    ///
    /// It only ever postpones. A delivery pass writes its lease into the same
    /// column, so a hold that moved a row *earlier* would hand the mail a
    /// second pass is composing to a third one, and the reader would get it
    /// twice.
    async fn hold_batch(
        &self,
        task_id: &str,
        recipient: &str,
        until: OffsetDateTime,
        cap: Duration,
    ) -> Result<()>;

    /// Whoever opened the task, unless they are a Viewer.
    async fn recipients_for_task_creator(&self, task_id: &str) -> Result<Vec<Recipient>>;

    /// One person by id, unless they are a Viewer — the subject of an
    /// activity line, for a rule that mails the person the line is about.
    async fn recipient(&self, user_id: &str) -> Result<Option<Recipient>>;
}
