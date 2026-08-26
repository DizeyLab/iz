//! Turso implementation of [`Store`].
//!
//! Turso is in-process and SQLite-compatible, so the schema is plain SQL and
//! there is no server to run alongside the binary. It ships no migration runner
//! of its own; [`TursoStore::open`] applies the numbered files in
//! `crates/izlek-core/migrations` at boot and records the version.

use async_trait::async_trait;
use rand::Rng;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use turso::transaction::TransactionBehavior;
use turso::{Builder, Connection, Row, Value, params};
use uuid::Uuid;

use super::secret;
use super::{
    ActivityLine, Attachment, Audience, Deletion, Event, Freeing, MailDecision, MailOutcome,
    MailRule, MailSend, NewAttachment, NewSender, NewTask, NewUser, Recipient, Result, SendKind,
    SendState, SenderTest, Session, SigninLink, Store, StoreError, Trigger, User, Workspace,
};
use crate::Role;
use crate::board::{BoardMeta, BoardReads, Column, Moved, Person, TaskRow, Transition};
use crate::detail::{
    ActivityEntry, ActivityKind, Comment, DeletionCost, DependencyEdge, DetailReads, FileLine,
    TaskFacts,
};
use time::Date;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;

/// Every migration, in order. Adding one means appending a file and a line.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../migrations/0001_init.sql")),
    (2, include_str!("../../migrations/0002_auth.sql")),
    (3, include_str!("../../migrations/0003_transition.sql")),
    (4, include_str!("../../migrations/0004_mail.sql")),
    (5, include_str!("../../migrations/0005_freeing.sql")),
    (6, include_str!("../../migrations/0006_sender_is_config.sql")),
    (7, include_str!("../../migrations/0007_who_invited.sql")),
    (8, include_str!("../../migrations/0008_sender_is_settings.sql")),
    (9, include_str!("../../migrations/0009_sender_test.sql")),
    (
        10,
        include_str!("../../migrations/0010_attachments_live_in_the_file.sql"),
    ),
    (
        11,
        include_str!("../../migrations/0011_what_the_mail_decided.sql"),
    ),
    (
        12,
        include_str!("../../migrations/0012_a_mail_that_owes_no_rule.sql"),
    ),
    (
        13,
        include_str!("../../migrations/0013_more_than_a_move.sql"),
    ),
];

/// The board a fresh workspace gets, and its columns. `Done` is the column
/// that stamps a card finished.
const DEFAULT_BOARD_NAME: &str = "Board";
const DEFAULT_COLUMNS: &[(&str, bool)] = &[
    ("Backlog", false),
    ("In Progress", false),
    ("Review", false),
    ("Done", true),
];

pub struct TursoStore {
    /// Shared by every single-statement call. Turso serialises statements on a
    /// connection, so this is safe; transactions are the exception and take a
    /// connection of their own.
    conn: Connection,
    db: turso::Database,
    /// Seals and opens `smtp_password`; see [`crate::store::secret`]. Never
    /// exposed through the `Store` trait — callers keep passing and receiving
    /// plaintext, this field is the detail that makes the row not be one.
    key: secret::Key,
}

impl TursoStore {
    /// Opens (creating if needed) the database at `path` and brings the schema
    /// up to date. `:memory:` gives a throwaway database for tests.
    pub async fn open(path: &str) -> Result<Self> {
        let existed = path != ":memory:" && std::path::Path::new(path).exists();
        let db = Builder::new_local(path).build().await.map_err(backend)?;
        let conn = db.connect().map_err(backend)?;
        // Turso is a single-writer engine. Two connections on one Database
        // handle serialise by themselves, but a second handle on the same file
        // (a second process, or a careless second open) fails outright with
        // "database is locked" and silently drops the write unless a busy
        // timeout is set. Both pragmas are set on every connection we hand out.
        for pragma in ["PRAGMA foreign_keys = ON", "PRAGMA busy_timeout = 5000"] {
            conn.execute(pragma, ()).await.map_err(backend)?;
        }
        // The database file holds a live SMTP credential (encrypted below, but
        // still worth not handing to every local user via umask default). 0600
        // right after creation closes the window between "file exists" and
        // "file is ours alone" — a WAL file, if this engine leaves one beside
        // the main file, gets the same restriction while it's still there to
        // restrict; if it does not exist yet, there is nothing to chmod and
        // nothing unprotected either, since it does not hold rows until a
        // write happens on a connection that already applied this.
        if !existed && path != ":memory:" {
            restrict_if_present(std::path::Path::new(path))?;
            restrict_if_present(&sibling(path, "-wal"))?;
            restrict_if_present(&sibling(path, "-shm"))?;
        }
        let key = load_key(path)?;
        let store = Self { conn, db, key };
        store.migrate().await?;
        store.encrypt_plaintext_passwords().await?;
        Ok(store)
    }

    /// The password-sealing step could not live in `migrations/`: encrypting a column in
    /// place needs the key, and the key is application state, not something
    /// SQL can reach. So the upgrade happens here instead, once per boot,
    /// idempotently — a value already carrying [`secret::is_sealed`]'s prefix
    /// is left alone, which is what makes running this every boot free after
    /// the first one. A deployment that predates this module has a plaintext
    /// password sitting in `workspace.smtp_password`; this is the one time it
    /// is read as plaintext, and only to seal it before anything else touches
    /// the row.
    async fn encrypt_plaintext_passwords(&self) -> Result<()> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, smtp_password FROM workspace WHERE smtp_password IS NOT NULL AND smtp_password <> ''",
                (),
            )
            .await
            .map_err(backend)?;
        let mut pending = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            let id = text(&row, 0)?;
            let password = text(&row, 1)?;
            if !secret::is_sealed(&password) {
                pending.push((id, password));
            }
        }
        for (id, plaintext) in pending {
            let sealed = secret::seal(&self.key, &plaintext);
            self.conn
                .execute(
                    "UPDATE workspace SET smtp_password = ?1 WHERE id = ?2",
                    params![sealed, id],
                )
                .await
                .map_err(backend)?;
        }
        Ok(())
    }

    async fn migrate(&self) -> Result<()> {
        self.conn
            .execute(
                "CREATE TABLE IF NOT EXISTS schema_version (
                     version    INTEGER PRIMARY KEY,
                     applied_at TEXT NOT NULL
                 )",
                (),
            )
            .await
            .map_err(backend)?;

        let applied = self.applied_versions().await?;
        for (version, sql) in MIGRATIONS {
            if applied.contains(version) {
                continue;
            }
            self.apply(*version, sql).await?;
        }
        Ok(())
    }

    /// One migration and the row that says it ran, in one transaction.
    ///
    /// A migration is not only a thing that can fail — 0005 rebuilds
    /// `mail_send` to drop a foreign key, and a crash between the DROP and the
    /// RENAME would leave the mail ledger gone rather than merely unmigrated.
    /// So the file and its `schema_version` row commit together or not at all,
    /// and a half-applied migration is a boot that starts over rather than a
    /// database with a hole in it.
    ///
    /// SQLite's DDL is transactional, which is what makes this possible at all.
    async fn apply(&self, version: i64, sql: &str) -> Result<()> {
        self.conn
            .execute("BEGIN IMMEDIATE", ())
            .await
            .map_err(backend)?;
        let written = async {
            self.conn.execute_batch(sql).await?;
            self.conn
                .execute(
                    "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                    params![
                        version,
                        now_text().map_err(|_| turso::Error::Misuse(
                            "the clock could not be read".to_string()
                        ))?
                    ],
                )
                .await?;
            Ok::<_, turso::Error>(())
        }
        .await;
        match written {
            Ok(()) => self
                .conn
                .execute("COMMIT", ())
                .await
                .map_err(backend)
                .map(|_| ()),
            Err(e) => {
                let _ = self.conn.execute("ROLLBACK", ()).await;
                Err(backend(e))
            }
        }
    }

    async fn applied_versions(&self) -> Result<Vec<i64>> {
        let mut rows = self
            .conn
            .query("SELECT version FROM schema_version", ())
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(row.get::<i64>(0).map_err(backend)?);
        }
        Ok(out)
    }

    /// The schema version the database is actually at.
    pub async fn schema_version(&self) -> Result<i64> {
        Ok(self
            .applied_versions()
            .await?
            .into_iter()
            .max()
            .unwrap_or(0))
    }

    /// A connection of its own, for work that opens a transaction.
    /// `Connection::transaction` takes `&mut self`, and a transaction on the
    /// shared connection would swallow everyone else's statements.
    async fn tx_conn(&self) -> Result<Connection> {
        let conn = self.db.connect().map_err(backend)?;
        for pragma in ["PRAGMA foreign_keys = ON", "PRAGMA busy_timeout = 5000"] {
            conn.execute(pragma, ()).await.map_err(backend)?;
        }
        Ok(conn)
    }

    async fn one_row(&self, sql: &str, args: impl turso::IntoParams) -> Result<Option<Row>> {
        let mut rows = self.conn.query(sql, args).await.map_err(backend)?;
        rows.next().await.map_err(backend)
    }
}

const RULE_COLUMNS: &str =
    "id, board_id, trigger_kind, trigger_column, subject, audience, enabled, created_at";

const SEND_COLUMNS: &str = "id, rule_id, event_id, task_id, recipient, state, attempts, \
     last_error, next_attempt_at, sent_at, kind, subject, body";

fn trigger_parts(trigger: &Trigger) -> (&'static str, Option<String>) {
    match trigger {
        Trigger::StatusBecomes(column) => ("status", Some(column.clone())),
        Trigger::Unblocked => ("unblocked", None),
        Trigger::Created => ("created", None),
        Trigger::Assigned => ("assigned", None),
        Trigger::Unassigned => ("unassigned", None),
        // No ActivityKind::Commented exists in `detail.rs` — a comment never
        // writes an activity line — so this word is chosen to match the
        // shape of the rest rather than lifted from `ActivityKind::as_str`.
        Trigger::Commented => ("commented", None),
        Trigger::DeadlineSet => ("deadline_set", None),
        Trigger::DeadlineCleared => ("deadline_cleared", None),
        Trigger::Retitled => ("retitled", None),
        Trigger::Linked => ("linked", None),
        Trigger::Unlinked => ("unlinked", None),
        Trigger::Deleted => ("deleted", None),
    }
}

fn audience_text(audience: Audience) -> &'static str {
    match audience {
        Audience::Assignees => "assignees",
        Audience::Board => "board",
        Audience::Creator => "creator",
    }
}

fn rule_from(row: &Row) -> Result<MailRule> {
    let kind = text(row, 2)?;
    let column = opt_text(row, 3)?;
    let trigger = match (kind.as_str(), column) {
        ("status", Some(column)) => Trigger::StatusBecomes(column),
        ("unblocked", None) => Trigger::Unblocked,
        ("created", None) => Trigger::Created,
        ("assigned", None) => Trigger::Assigned,
        ("unassigned", None) => Trigger::Unassigned,
        ("commented", None) => Trigger::Commented,
        ("deadline_set", None) => Trigger::DeadlineSet,
        ("deadline_cleared", None) => Trigger::DeadlineCleared,
        ("retitled", None) => Trigger::Retitled,
        ("linked", None) => Trigger::Linked,
        ("unlinked", None) => Trigger::Unlinked,
        ("deleted", None) => Trigger::Deleted,
        (kind, _) => return Err(StoreError::Corrupt(format!("mail rule trigger {kind:?}"))),
    };
    let audience = match text(row, 5)?.as_str() {
        "assignees" => Audience::Assignees,
        "board" => Audience::Board,
        "creator" => Audience::Creator,
        other => return Err(StoreError::Corrupt(format!("mail rule audience {other:?}"))),
    };
    Ok(MailRule {
        id: text(row, 0)?,
        board_id: text(row, 1)?,
        trigger,
        subject: text(row, 4)?,
        audience,
        enabled: row.get::<i64>(6).map_err(backend)? != 0,
        created_at: parse_stamp(&text(row, 7)?)?,
    })
}

fn send_from(row: &Row) -> Result<MailSend> {
    let state = match text(row, 5)?.as_str() {
        "pending" => SendState::Pending,
        "sent" => SendState::Sent,
        "failed" => SendState::Failed,
        "abandoned" => SendState::Abandoned,
        other => return Err(StoreError::Corrupt(format!("send state {other:?}"))),
    };
    let kind_raw = text(row, 10)?;
    let kind = SendKind::parse(&kind_raw)
        .ok_or_else(|| StoreError::Corrupt(format!("send kind {kind_raw:?}")))?;
    Ok(MailSend {
        id: text(row, 0)?,
        rule_id: opt_text(row, 1)?,
        event_id: opt_text(row, 2)?,
        task_id: opt_text(row, 3)?,
        recipient: text(row, 4)?,
        state,
        attempts: row.get::<i64>(6).map_err(backend)?.max(0) as u32,
        last_error: opt_text(row, 7)?,
        next_attempt_at: opt_stamp(row, 8)?,
        sent_at: opt_stamp(row, 9)?,
        kind,
        subject: opt_text(row, 11)?,
        body: opt_text(row, 12)?,
    })
}

const DECISION_COLUMNS: &str = "id, rule_id, event_id, task_id, outcome, detail, created_at";

fn decision_from(row: &Row) -> Result<MailDecision> {
    let raw = text(row, 4)?;
    let outcome =
        MailOutcome::parse(&raw).ok_or_else(|| StoreError::Corrupt(format!("mail outcome {raw:?}")))?;
    Ok(MailDecision {
        id: text(row, 0)?,
        rule_id: text(row, 1)?,
        event_id: text(row, 2)?,
        task_id: text(row, 3)?,
        outcome,
        detail: text(row, 5)?,
        at: parse_stamp(&text(row, 6)?)?,
    })
}

async fn recipients_from(rows: &mut turso::Rows) -> Result<Vec<Recipient>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(backend)? {
        out.push(Recipient {
            user_id: text(&row, 0)?,
            email: text(&row, 1)?,
            display_name: text(&row, 2)?,
        });
    }
    Ok(out)
}

/// `path` with its file name suffixed, for the WAL/SHM files Turso may leave
/// beside the main one (`izlek.db-wal`, `izlek.db-shm`).
fn sibling(path: &str, suffix: &str) -> std::path::PathBuf {
    let mut name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    std::path::Path::new(path).with_file_name(name)
}

/// 0600 on `path` if it exists, a no-op if it does not — chmodding a file
/// into existence would be worse than leaving it be, since it would exist
/// with no rows in it yet regardless.
fn restrict_if_present(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        secret::restrict(path).map_err(backend)?;
    }
    Ok(())
}

/// The key for `path`'s database. `:memory:` has no directory to anchor a
/// sibling file to and does not survive the process anyway, so it gets a
/// key generated fresh in memory — every in-memory store in the test suite
/// is its own, unrelated encryption domain, which is exactly right for a
/// database that is gone the moment the handle is dropped.
fn load_key(path: &str) -> Result<secret::Key> {
    if path == ":memory:" {
        let mut key = [0u8; secret::KEY_BYTES];
        rand::rng().fill_bytes(&mut key);
        return Ok(key);
    }
    let dir = std::path::Path::new(path)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    secret::load_or_create_key(&dir.join("izlek.key")).map_err(backend)
}

fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Turso reports constraint failures as text; there is no typed error to match
/// on in 0.8.0-pre.7.
fn is_constraint_violation(e: &turso::Error) -> bool {
    let text = e.to_string().to_lowercase();
    text.contains("constraint") || text.contains("unique")
}

fn now_text() -> Result<String> {
    stamp(OffsetDateTime::now_utc())
}

/// What the body of a move transaction decided, before the commit or rollback
/// that acts on it.
enum Outcome {
    Wrote,
    Stale,
    Missing,
}

fn stamp(at: OffsetDateTime) -> Result<String> {
    at.format(&Rfc3339)
        .map_err(|e| StoreError::Corrupt(format!("timestamp: {e}")))
}

fn parse_stamp(raw: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .map_err(|e| StoreError::Corrupt(format!("timestamp {raw:?}: {e}")))
}

/// Deadlines are days, not instants: a deadline is the same day wherever the
/// person reading the card is.
const DAY: &[BorrowedFormatItem] = format_description!("[year]-[month]-[day]");

fn day_text(day: Date) -> Result<String> {
    day.format(&DAY)
        .map_err(|e| StoreError::Corrupt(format!("deadline: {e}")))
}

fn opt_day(row: &Row, idx: usize) -> Result<Option<Date>> {
    match row.get::<Option<String>>(idx).map_err(backend)? {
        Some(raw) => Date::parse(&raw, &DAY)
            .map(Some)
            .map_err(|e| StoreError::Corrupt(format!("deadline {raw:?}: {e}"))),
        None => Ok(None),
    }
}

fn opt_stamp(row: &Row, idx: usize) -> Result<Option<OffsetDateTime>> {
    match row.get::<Option<String>>(idx).map_err(backend)? {
        Some(raw) => Ok(Some(parse_stamp(&raw)?)),
        None => Ok(None),
    }
}

/// One attachment row, in the column order every query above selects.
fn attachment_row(row: &Row) -> Result<Attachment> {
    Ok(Attachment {
        id: text(row, 0)?,
        task_id: text(row, 1)?,
        comment_id: opt_text(row, 2)?,
        file_name: text(row, 3)?,
        mime_type: text(row, 4)?,
        size_bytes: row.get::<i64>(5).map_err(backend)?.max(0) as u64,
        uploaded_by: text(row, 6)?,
        uploaded_at: parse_stamp(&text(row, 7)?)?,
    })
}

fn text(row: &Row, idx: usize) -> Result<String> {
    row.get::<String>(idx).map_err(backend)
}

fn opt_text(row: &Row, idx: usize) -> Result<Option<String>> {
    row.get::<Option<String>>(idx).map_err(backend)
}

fn count_of(row: &Row) -> Result<u64> {
    row.get::<i64>(0).map_err(backend).map(|n| n.max(0) as u64)
}

fn workspace_from(row: &Row) -> Result<Workspace> {
    let types_json = text(row, 4)?;
    let allowed_file_types: Vec<String> = if types_json.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&types_json)
            .map_err(|e| StoreError::Corrupt(format!("allowed_file_types: {e}")))?
    };
    Ok(Workspace {
        id: text(row, 0)?,
        name: text(row, 1)?,
        created_at: parse_stamp(&text(row, 2)?)?,
        attachment_limit_bytes: row.get::<i64>(3).map_err(backend)?.max(0) as u64,
        allowed_file_types,
        photo_limit_bytes: row.get::<i64>(5).map_err(backend)?.max(0) as u64,
        smtp_host: opt_text(row, 6)?,
        smtp_port: row.get::<Option<u32>>(7).map_err(backend)?,
        smtp_username: opt_text(row, 8)?,
        smtp_from_name: opt_text(row, 9)?,
        smtp_from_address: opt_text(row, 10)?,
        smtp_password_set: row.get::<i64>(11).map_err(backend)? != 0,
        sender_test: match opt_stamp(row, 12)? {
            Some(at) => Some(SenderTest {
                at,
                took_ms: row.get::<i64>(13).map_err(backend)?.max(0) as u64,
                error: opt_text(row, 14)?,
            }),
            None => None,
        },
    })
}

// The password is not among these, and the last entry is why: the query asks
// the database whether a password exists and carries back a 0 or a 1. The value
// never leaves the row, so no caller can pass it on by accident.
const WORKSPACE_COLUMNS: &str = "id, name, created_at, attachment_limit_bytes, \
     allowed_file_types, photo_limit_bytes, smtp_host, smtp_port, smtp_username, \
     smtp_from_name, smtp_from_address, \
     (smtp_password IS NOT NULL AND smtp_password <> ''), \
     smtp_test_at, smtp_test_ms, smtp_test_error";

fn user_from(row: &Row) -> Result<User> {
    Ok(User {
        id: text(row, 0)?,
        workspace_id: text(row, 1)?,
        email: text(row, 2)?,
        display_name: text(row, 3)?,
        role: Role::parse(&text(row, 4)?).ok_or_else(|| StoreError::Corrupt("role".into()))?,
        password_hash: opt_text(row, 5)?,
        photo_path: opt_text(row, 6)?,
        created_at: parse_stamp(&text(row, 7)?)?,
        last_signed_in_at: opt_stamp(row, 8)?,
        invited_by: opt_text(row, 9)?,
    })
}

const USER_COLUMNS: &str = "id, workspace_id, email, display_name, role, password_hash, \
     photo_path, created_at, last_signed_in_at, invited_by";

fn signin_link_from(row: &Row) -> Result<SigninLink> {
    Ok(SigninLink {
        id: text(row, 0)?,
        user_id: text(row, 1)?,
        created_at: parse_stamp(&text(row, 2)?)?,
        expires_at: parse_stamp(&text(row, 3)?)?,
        used_at: opt_stamp(row, 4)?,
    })
}

fn session_from(row: &Row) -> Result<Session> {
    Ok(Session {
        id: text(row, 0)?,
        user_id: text(row, 1)?,
        created_at: parse_stamp(&text(row, 2)?)?,
        expires_at: parse_stamp(&text(row, 3)?)?,
        revoked_at: opt_stamp(row, 4)?,
    })
}

const SESSION_COLUMNS: &str = "id, user_id, created_at, expires_at, revoked_at";

/// Addresses are matched case-insensitively; the display form is kept as typed.
fn fold_email(email: &str) -> String {
    email.trim().to_lowercase()
}

#[async_trait]
impl Store for TursoStore {
    async fn claim_workspace(
        &self,
        workspace_name: &str,
        email: &str,
        display_name: &str,
        password_hash: &str,
    ) -> Result<(Workspace, User)> {
        let workspace_id = Uuid::new_v4().to_string();
        let admin_id = Uuid::new_v4().to_string();
        let board_id = Uuid::new_v4().to_string();
        let now = now_text()?;
        let email = fold_email(email);

        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: take the write lock at BEGIN, so a second claimant waits
        // out the busy timeout here instead of doing the whole insert and
        // discovering the conflict at the end.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let claimed = async {
            tx.execute(
                "INSERT INTO workspace (id, name, created_at) VALUES (?1, ?2, ?3)",
                params![workspace_id.clone(), workspace_name, now.clone()],
            )
            .await?;
            tx.execute(
                "INSERT INTO user (id, workspace_id, email, display_name, role, password_hash, \
                 created_at) VALUES (?1, ?2, ?3, ?4, 'admin', ?5, ?6)",
                params![
                    admin_id.clone(),
                    workspace_id.clone(),
                    email,
                    display_name,
                    password_hash,
                    now.clone()
                ],
            )
            .await?;
            // The claim itself. Fixed primary key, so the second writer loses.
            tx.execute(
                "INSERT INTO workspace_owner (singleton, user_id, claimed_at) \
                 VALUES (1, ?1, ?2)",
                params![admin_id.clone(), now.clone()],
            )
            .await?;
            // A claimed workspace is never boardless: the EmptyBoard screen is
            // four named columns waiting for a first card, not a setup step.
            tx.execute(
                "INSERT INTO board (id, workspace_id, name, task_prefix, created_at) \
                 VALUES (?1, ?2, ?3, 'DZ', ?4)",
                params![
                    board_id.clone(),
                    workspace_id.clone(),
                    DEFAULT_BOARD_NAME,
                    now
                ],
            )
            .await?;
            for (position, (name, is_done)) in DEFAULT_COLUMNS.iter().enumerate() {
                tx.execute(
                    "INSERT INTO board_column (id, board_id, name, position, is_done) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        Uuid::new_v4().to_string(),
                        board_id.clone(),
                        *name,
                        position as i64,
                        i64::from(*is_done)
                    ],
                )
                .await?;
            }
            Ok::<_, turso::Error>(())
        }
        .await;

        match claimed {
            Ok(()) => tx.commit().await.map_err(|e| {
                if is_constraint_violation(&e) {
                    StoreError::AlreadyClaimed
                } else {
                    backend(e)
                }
            })?,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(if is_constraint_violation(&e) {
                    StoreError::AlreadyClaimed
                } else {
                    backend(e)
                });
            }
        }

        let workspace = self.workspace().await?.ok_or(StoreError::NotFound)?;
        let admin = self.user(&admin_id).await?.ok_or(StoreError::NotFound)?;
        Ok((workspace, admin))
    }

    async fn owner(&self) -> Result<Option<User>> {
        let sql = format!(
            "SELECT {USER_COLUMNS} FROM user \
             WHERE id = (SELECT user_id FROM workspace_owner WHERE singleton = 1)"
        );
        match self.one_row(&sql, ()).await? {
            Some(row) => Ok(Some(user_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn workspace(&self) -> Result<Option<Workspace>> {
        let sql = format!("SELECT {WORKSPACE_COLUMNS} FROM workspace LIMIT 1");
        match self.one_row(&sql, ()).await? {
            Some(row) => Ok(Some(workspace_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn set_sender(&self, workspace_id: &str, sender: NewSender) -> Result<()> {
        // `COALESCE(?4, smtp_password)` is the write-only field made real: the
        // screen sends no password when the admin did not type one, and the
        // stored secret survives an edit to the port. Passing an empty string
        // is not a way to blank it either — the form sends `None` for empty.
        //
        // The last test result is cleared by the same statement. It was about
        // the settings that have just been replaced, and a green "delivered"
        // line under a host nobody has tried yet is worse than no line at all.
        // Sealed here, not by the caller — the `Store` trait keeps passing
        // plaintext so nothing outside this file needs to know a cipher
        // exists. See `crate::store::secret`.
        let sealed_password = sender.password.as_deref().map(|p| secret::seal(&self.key, p));
        self.conn
            .execute(
                "UPDATE workspace SET smtp_host = ?1, smtp_port = ?2, smtp_username = ?3, \
                 smtp_password = COALESCE(?4, smtp_password), smtp_from_name = ?5, \
                 smtp_from_address = ?6, smtp_test_at = NULL, smtp_test_ms = NULL, \
                 smtp_test_error = NULL WHERE id = ?7",
                params![
                    sender.host,
                    sender.port as i64,
                    sender.username,
                    sealed_password,
                    sender.from_name,
                    sender.from_address,
                    workspace_id
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn record_sender_test(&self, workspace_id: &str, test: SenderTest) -> Result<()> {
        self.conn
            .execute(
                "UPDATE workspace SET smtp_test_at = ?1, smtp_test_ms = ?2, \
                 smtp_test_error = ?3 WHERE id = ?4",
                params![
                    stamp(test.at)?,
                    test.took_ms as i64,
                    test.error,
                    workspace_id
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn smtp_password(&self, workspace_id: &str) -> Result<Option<String>> {
        let sealed = match self
            .one_row(
                "SELECT smtp_password FROM workspace WHERE id = ?1",
                params![workspace_id],
            )
            .await?
        {
            Some(row) => opt_text(&row, 0)?,
            None => return Err(StoreError::NotFound),
        };
        // A decrypt failure — wrong key, damaged ciphertext, a restored
        // backup missing its `izlek.key` — comes back as `None` rather than
        // an error. To every caller that is indistinguishable from a
        // workspace that never had a sender, which is the intended
        // degradation: mail send falls back to the no-sender path instead of
        // crashing or poisoning the settings screen, and the admin retypes
        // the password once through the existing write-only field to heal it.
        Ok(sealed.as_deref().and_then(|s| secret::open(&self.key, s)))
    }

    async fn set_limits(
        &self,
        workspace_id: &str,
        attachment_limit_bytes: u64,
        photo_limit_bytes: u64,
        allowed_file_types: &[String],
    ) -> Result<()> {
        let types = serde_json::to_string(allowed_file_types)
            .map_err(|e| StoreError::Corrupt(format!("allowed_file_types: {e}")))?;
        self.conn
            .execute(
                "UPDATE workspace SET attachment_limit_bytes = ?1, photo_limit_bytes = ?2, \
                 allowed_file_types = ?3 WHERE id = ?4",
                params![
                    attachment_limit_bytes as i64,
                    photo_limit_bytes as i64,
                    types,
                    workspace_id
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn create_user(&self, new: NewUser) -> Result<User> {
        let email = fold_email(&new.email);
        if self
            .user_by_email(&new.workspace_id, &email)
            .await?
            .is_some()
        {
            return Err(StoreError::Conflict("account"));
        }
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO user \
                 (id, workspace_id, email, display_name, role, created_at, invited_by) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id.clone(),
                    new.workspace_id,
                    email,
                    new.display_name,
                    new.role.as_str(),
                    now_text()?,
                    new.invited_by
                ],
            )
            .await
            .map_err(backend)?;
        self.user(&id).await?.ok_or(StoreError::NotFound)
    }

    async fn user(&self, id: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {USER_COLUMNS} FROM user WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => Ok(Some(user_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn user_by_email(&self, workspace_id: &str, email: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {USER_COLUMNS} FROM user WHERE workspace_id = ?1 AND email = ?2");
        match self
            .one_row(&sql, params![workspace_id, fold_email(email)])
            .await?
        {
            Some(row) => Ok(Some(user_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn users(&self, workspace_id: &str) -> Result<Vec<User>> {
        let sql = format!(
            "SELECT {USER_COLUMNS} FROM user WHERE workspace_id = ?1 ORDER BY created_at, id"
        );
        let mut rows = self
            .conn
            .query(&sql, params![workspace_id])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(user_from(&row)?);
        }
        Ok(out)
    }

    async fn count_users(&self, workspace_id: &str) -> Result<u64> {
        match self
            .one_row(
                "SELECT COUNT(*) FROM user WHERE workspace_id = ?1",
                params![workspace_id],
            )
            .await?
        {
            Some(row) => count_of(&row),
            None => Ok(0),
        }
    }

    async fn set_password_hash(&self, user_id: &str, hash: &str) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE user SET password_hash = ?1 WHERE id = ?2",
                params![hash, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn set_profile(
        &self,
        user_id: &str,
        display_name: &str,
        photo_path: Option<&str>,
    ) -> Result<()> {
        let photo = match photo_path {
            Some(p) => Value::from(p.to_string()),
            None => Value::Null,
        };
        let n = self
            .conn
            .execute(
                "UPDATE user SET display_name = ?1, photo_path = ?2 WHERE id = ?3",
                params![display_name, photo, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn set_role(&self, user_id: &str, role: Role) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE user SET role = ?1 WHERE id = ?2",
                params![role.as_str(), user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn mark_signed_in(&self, user_id: &str, at: OffsetDateTime) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE user SET last_signed_in_at = ?1 WHERE id = ?2",
                params![stamp(at)?, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn create_signin_link(
        &self,
        user_id: &str,
        token_hash: &str,
        expires_at: OffsetDateTime,
    ) -> Result<SigninLink> {
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO signin_link (id, user_id, token_hash, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.clone(),
                    user_id,
                    token_hash,
                    now_text()?,
                    stamp(expires_at)?
                ],
            )
            .await
            .map_err(backend)?;
        match self
            .one_row(
                "SELECT id, user_id, created_at, expires_at, used_at FROM signin_link WHERE id = ?1",
                params![id],
            )
            .await?
        {
            Some(row) => signin_link_from(&row),
            None => Err(StoreError::NotFound),
        }
    }

    async fn signin_link_by_hash(&self, token_hash: &str) -> Result<Option<SigninLink>> {
        match self
            .one_row(
                "SELECT id, user_id, created_at, expires_at, used_at FROM signin_link \
                 WHERE token_hash = ?1",
                params![token_hash],
            )
            .await?
        {
            Some(row) => Ok(Some(signin_link_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn consume_signin_link(&self, id: &str, at: OffsetDateTime) -> Result<bool> {
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        // Conditional update plus rows-affected, never read-then-write: the
        // second redemption of a prefetched link updates nothing.
        let n = tx
            .execute(
                "UPDATE signin_link SET used_at = ?1 WHERE id = ?2 AND used_at IS NULL",
                params![stamp(at)?, id],
            )
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(n == 1)
    }

    async fn create_session(
        &self,
        user_id: &str,
        token_hash: &str,
        expires_at: OffsetDateTime,
    ) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO session (id, user_id, token_hash, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.clone(),
                    user_id,
                    token_hash,
                    now_text()?,
                    stamp(expires_at)?
                ],
            )
            .await
            .map_err(backend)?;
        let sql = format!("SELECT {SESSION_COLUMNS} FROM session WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => session_from(&row),
            None => Err(StoreError::NotFound),
        }
    }

    async fn session_by_hash(&self, token_hash: &str) -> Result<Option<Session>> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM session WHERE token_hash = ?1");
        match self.one_row(&sql, params![token_hash]).await? {
            Some(row) => Ok(Some(session_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn session_token_hash(&self, id: &str) -> Result<Option<String>> {
        match self
            .one_row("SELECT token_hash FROM session WHERE id = ?1", params![id])
            .await?
        {
            Some(row) => Ok(Some(text(&row, 0)?)),
            None => Ok(None),
        }
    }

    async fn revoke_session(&self, id: &str, at: OffsetDateTime) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE session SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                params![stamp(at)?, id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn revoke_sessions_for_user(&self, user_id: &str, at: OffsetDateTime) -> Result<u64> {
        self.conn
            .execute(
                "UPDATE session SET revoked_at = ?1 WHERE user_id = ?2 AND revoked_at IS NULL",
                params![stamp(at)?, user_id],
            )
            .await
            .map_err(backend)
    }

    async fn record_auth_attempt(&self, bucket: &str, at: OffsetDateTime) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO auth_attempt (id, bucket, attempted_at) VALUES (?1, ?2, ?3)",
                params![Uuid::new_v4().to_string(), bucket, stamp(at)?],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn count_auth_attempts(&self, bucket: &str, since: OffsetDateTime) -> Result<u64> {
        // RFC 3339 in UTC sorts lexicographically, which is why the timestamps
        // are stored in that shape.
        match self
            .one_row(
                "SELECT COUNT(*) FROM auth_attempt WHERE bucket = ?1 AND attempted_at >= ?2",
                params![bucket, stamp(since)?],
            )
            .await?
        {
            Some(row) => count_of(&row),
            None => Ok(0),
        }
    }

    async fn clear_auth_attempts(&self, bucket: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM auth_attempt WHERE bucket = ?1",
                params![bucket],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn prune_auth_attempts(&self, before: OffsetDateTime) -> Result<u64> {
        self.conn
            .execute(
                "DELETE FROM auth_attempt WHERE attempted_at < ?1",
                params![stamp(before)?],
            )
            .await
            .map_err(backend)
    }

    // -- board -------------------------------------------------------------

    async fn create_column(
        &self,
        board_id: &str,
        name: &str,
        position: i64,
        is_done: bool,
    ) -> Result<Column> {
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO board_column (id, board_id, name, position, is_done) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id.clone(), board_id, name, position, i64::from(is_done)],
            )
            .await
            .map_err(backend)?;
        Ok(Column {
            id,
            name: name.to_string(),
            position,
            is_done,
        })
    }

    async fn create_task(&self, new: NewTask<'_>) -> Result<TaskRow> {
        let id = Uuid::new_v4().to_string();
        let now = now_text()?;
        let deadline = new.deadline.map(day_text).transpose()?;

        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: the key comes off a counter, so the read of next_task_no
        // and the write that bumps it must not be interleaved with another
        // writer's pair.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let written = async {
            let mut rows = tx
                .query(
                    "SELECT task_prefix, next_task_no FROM board WHERE id = ?1",
                    params![new.board_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(None);
            };
            let prefix = row.get::<String>(0)?;
            let number = row.get::<i64>(1)?;
            drop(rows);
            tx.execute(
                "UPDATE board SET next_task_no = ?1 WHERE id = ?2",
                params![number + 1, new.board_id],
            )
            .await?;

            // New cards land at the end of their column.
            let mut rows = tx
                .query(
                    "SELECT COALESCE(MAX(position), 0) FROM task WHERE column_id = ?1",
                    params![new.column_id],
                )
                .await?;
            let last = match rows.next().await? {
                Some(row) => row.get::<f64>(0).unwrap_or(0.0),
                None => 0.0,
            };
            drop(rows);
            let position = last + 1.0;

            let task_key = format!("{prefix}-{number:02}");
            tx.execute(
                "INSERT INTO task (id, board_id, task_key, title, description, column_id, \
                 deadline, position, created_by, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    id.clone(),
                    new.board_id,
                    task_key.clone(),
                    new.title,
                    new.description,
                    new.column_id,
                    deadline.clone(),
                    position,
                    new.created_by,
                    now.clone()
                ],
            )
            .await?;
            tx.execute(
                "INSERT INTO activity (id, task_id, actor_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, ?4, '', ?5)",
                params![
                    Uuid::new_v4().to_string(),
                    id.clone(),
                    new.created_by,
                    ActivityKind::Created.as_str(),
                    now.clone()
                ],
            )
            .await?;
            Ok::<_, turso::Error>(Some((task_key, position)))
        }
        .await;

        let written = match written {
            Ok(Some(written)) => written,
            Ok(None) => {
                let _ = tx.rollback().await;
                return Err(StoreError::NotFound);
            }
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(backend(e));
            }
        };
        tx.commit().await.map_err(backend)?;

        let (task_key, position) = written;
        Ok(TaskRow {
            id,
            task_key,
            title: new.title.to_string(),
            column_id: new.column_id.to_string(),
            deadline: new.deadline,
            position,
            done_at: None,
        })
    }

    async fn assign_task(&self, task_id: &str, user_id: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO task_assignee (task_id, user_id) VALUES (?1, ?2)",
                params![task_id, user_id],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn unassign_task(&self, task_id: &str, user_id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM task_assignee WHERE task_id = ?1 AND user_id = ?2",
                params![task_id, user_id],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn add_dependency(
        &self,
        blocked_task_id: &str,
        blocking_task_id: &str,
        at: OffsetDateTime,
    ) -> Result<()> {
        if blocked_task_id == blocking_task_id {
            return Err(StoreError::Cycle);
        }
        let stamp = stamp(at)?;

        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: the reachability read and the insert that invalidates it
        // must not be interleaved with another writer's pair, or two adds each
        // pass a check that was true when they read it and the pair closes a
        // circle neither of them could see.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let written = async {
            // Live edges only. `cleared_at` is set by an unlink and nothing
            // else, so a cleared edge is a link that was removed — walking it
            // refuses a link the screen no longer shows as a circle. A link
            // whose blocker is merely finished still walks: that is `done_at`,
            // and the edge is still in force.
            let mut rows = tx
                .query(
                    "SELECT d.blocked_task_id, d.blocking_task_id FROM task_dependency d \
                     JOIN task b ON b.id = d.blocked_task_id \
                     JOIN task k ON k.id = d.blocking_task_id \
                     WHERE d.cleared_at IS NULL \
                     AND b.deleted_at IS NULL AND k.deleted_at IS NULL",
                    (),
                )
                .await?;
            // blocked -> [what blocks it]
            let mut edges: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            while let Some(row) = rows.next().await? {
                edges
                    .entry(row.get::<String>(0)?)
                    .or_default()
                    .push(row.get::<String>(1)?);
            }
            drop(rows);

            // Walking "what blocks it" from the proposed blocker: if that walk
            // reaches the task being blocked, the new edge closes the loop.
            let mut seen = std::collections::HashSet::new();
            let mut queue = std::collections::VecDeque::new();
            queue.push_back(blocking_task_id.to_string());
            while let Some(node) = queue.pop_front() {
                if node == blocked_task_id {
                    return Ok(false);
                }
                if !seen.insert(node.clone()) {
                    continue;
                }
                for next in edges.get(&node).into_iter().flatten() {
                    queue.push_back(next.clone());
                }
            }

            // Re-adding a dependency that was cleared puts it back in force
            // rather than leaving the cleared row to hide it.
            tx.execute(
                "INSERT INTO task_dependency (blocked_task_id, blocking_task_id, created_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT (blocked_task_id, blocking_task_id) \
                 DO UPDATE SET created_at = ?3, cleared_at = NULL",
                params![blocked_task_id, blocking_task_id, stamp.clone()],
            )
            .await?;
            Ok::<_, turso::Error>(true)
        }
        .await;

        match written {
            Ok(true) => tx.commit().await.map_err(backend),
            Ok(false) => {
                let _ = tx.rollback().await;
                Err(StoreError::Cycle)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(backend(e))
            }
        }
    }

    async fn clear_dependency(
        &self,
        blocked_task_id: &str,
        blocking_task_id: &str,
        at: OffsetDateTime,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE task_dependency SET cleared_at = ?3 \
                 WHERE blocked_task_id = ?1 AND blocking_task_id = ?2 AND cleared_at IS NULL",
                params![blocked_task_id, blocking_task_id, stamp(at)?],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn add_comment(
        &self,
        task_id: &str,
        author_id: &str,
        body: &str,
        at: OffsetDateTime,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO comment (id, task_id, author_id, body, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id.clone(), task_id, author_id, body, stamp(at)?],
            )
            .await
            .map_err(backend)?;
        Ok(id)
    }

    async fn add_attachment(&self, new: NewAttachment<'_>) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let size = new.bytes.len() as i64;
        self.conn
            .execute(
                "INSERT INTO attachment (id, task_id, comment_id, file_name, mime_type, \
                 size_bytes, bytes, uploaded_by, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id.clone(),
                    new.task_id,
                    new.comment_id,
                    new.file_name,
                    new.mime_type,
                    size,
                    new.bytes,
                    new.uploaded_by,
                    stamp(new.at)?
                ],
            )
            .await
            .map_err(backend)?;
        Ok(id)
    }

    async fn attachments(&self, task_id: &str) -> Result<Vec<Attachment>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, task_id, comment_id, file_name, mime_type, size_bytes, \
                 uploaded_by, created_at FROM attachment \
                 WHERE task_id = ?1 ORDER BY created_at, rowid",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(attachment_row(&row)?);
        }
        Ok(out)
    }

    async fn attachment(&self, id: &str) -> Result<Option<Attachment>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, task_id, comment_id, file_name, mime_type, size_bytes, \
                 uploaded_by, created_at FROM attachment WHERE id = ?1",
                params![id],
            )
            .await
            .map_err(backend)?;
        match rows.next().await.map_err(backend)? {
            Some(row) => Ok(Some(attachment_row(&row)?)),
            None => Ok(None),
        }
    }

    async fn attachment_bytes(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let mut rows = self
            .conn
            .query("SELECT bytes FROM attachment WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        match rows.next().await.map_err(backend)? {
            Some(row) => Ok(Some(row.get::<Vec<u8>>(0).map_err(backend)?)),
            None => Ok(None),
        }
    }

    async fn delete_attachment(&self, id: &str) -> Result<bool> {
        let gone = self
            .conn
            .execute("DELETE FROM attachment WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        Ok(gone > 0)
    }

    async fn save_task(
        &self,
        task_id: &str,
        title: &str,
        description: &str,
        deadline: Option<Date>,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<()> {
        let deadline = deadline.map(day_text).transpose()?;
        let stamp = stamp(at)?;

        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: the activity lines say what changed, so they are decided
        // from the row this write is replacing and must see it unchanged.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let written = async {
            let mut rows = tx
                .query(
                    "SELECT title, description, deadline FROM task \
                     WHERE id = ?1 AND deleted_at IS NULL",
                    params![task_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(false);
            };
            let was_title = row.get::<String>(0)?;
            let was_description = row.get::<String>(1)?;
            let was_deadline = row.get::<Option<String>>(2)?;
            drop(rows);

            tx.execute(
                "UPDATE task SET title = ?2, description = ?3, deadline = ?4, updated_at = ?5 \
                 WHERE id = ?1",
                params![task_id, title, description, deadline.clone(), stamp.clone()],
            )
            .await?;

            let mut lines: Vec<(&'static str, String)> = Vec::new();
            if was_title != title {
                lines.push((ActivityKind::Retitled.as_str(), title.to_string()));
            }
            if was_description != description {
                lines.push((ActivityKind::Described.as_str(), String::new()));
            }
            if was_deadline != deadline {
                match &deadline {
                    Some(day) => lines.push((ActivityKind::DeadlineSet.as_str(), day.clone())),
                    None => lines.push((ActivityKind::DeadlineCleared.as_str(), String::new())),
                }
            }
            for (kind, detail) in lines {
                tx.execute(
                    "INSERT INTO activity (id, task_id, actor_id, kind, detail, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        Uuid::new_v4().to_string(),
                        task_id,
                        actor_id,
                        kind,
                        detail,
                        stamp.clone()
                    ],
                )
                .await?;
            }
            Ok::<_, turso::Error>(true)
        }
        .await;

        match written {
            Ok(true) => tx.commit().await.map_err(backend),
            Ok(false) => {
                let _ = tx.rollback().await;
                Err(StoreError::NotFound)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(backend(e))
            }
        }
    }

    async fn move_task(
        &self,
        task_id: &str,
        from_column_id: &str,
        to_column_id: &str,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<Moved> {
        let stamp = stamp(at)?;
        let transition_id = Uuid::new_v4().to_string();

        // Dropping a card back where it came from is not a move. Answer before
        // opening a transaction: there is nothing to serialise.
        if from_column_id == to_column_id {
            return Ok(Moved::Unchanged);
        }

        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: the whole point is that two drops on the same card cannot
        // both read "it is in Backlog" and both write a crossing out of it.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let written = async {
            let mut rows = tx
                .query(
                    "SELECT board_id, column_id FROM task \
                     WHERE id = ?1 AND deleted_at IS NULL",
                    params![task_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(Outcome::Missing);
            };
            let board_id = row.get::<String>(0)?;
            let sitting_in = row.get::<String>(1)?;
            drop(rows);

            // Somebody moved it while the drag was in the air.
            if sitting_in != from_column_id {
                return Ok(Outcome::Stale);
            }

            // The destination has to be a column of this task's own board: a
            // column id arrives in a form, and a form is not a promise.
            let mut rows = tx
                .query(
                    "SELECT name, is_done FROM board_column WHERE id = ?1 AND board_id = ?2",
                    params![to_column_id, board_id.clone()],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(Outcome::Missing);
            };
            let to_name = row.get::<String>(0)?;
            let to_is_done = row.get::<i64>(1)? != 0;
            drop(rows);

            let mut rows = tx
                .query(
                    "SELECT name FROM board_column WHERE id = ?1",
                    params![from_column_id],
                )
                .await?;
            let from_name = match rows.next().await? {
                Some(row) => row.get::<String>(0)?,
                None => from_column_id.to_string(),
            };
            drop(rows);

            // The card lands at the end of its new column. Where inside a
            // column a card sits is the board's sort control's business, not
            // the drop's.
            let mut rows = tx
                .query(
                    "SELECT COALESCE(MAX(position), 0) + 1 FROM task \
                     WHERE column_id = ?1 AND deleted_at IS NULL",
                    params![to_column_id],
                )
                .await?;
            let landing = match rows.next().await? {
                Some(row) => row.get::<f64>(0)?,
                None => 1.0,
            };
            drop(rows);

            // Conditional on the column read above, so this is still one
            // writer even if the IMMEDIATE lock were ever relaxed.
            let moved = tx
                .execute(
                    "UPDATE task SET column_id = ?2, position = ?3, done_at = ?4, updated_at = ?5 \
                     WHERE id = ?1 AND column_id = ?6 AND deleted_at IS NULL",
                    params![
                        task_id,
                        to_column_id,
                        landing,
                        if to_is_done {
                            Some(stamp.clone())
                        } else {
                            None
                        },
                        stamp.clone(),
                        from_column_id
                    ],
                )
                .await?;
            if moved == 0 {
                return Ok(Outcome::Stale);
            }

            tx.execute(
                "INSERT INTO transition \
                 (id, task_id, from_column, to_column, actor_id, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    transition_id.clone(),
                    task_id,
                    from_column_id,
                    to_column_id,
                    actor_id,
                    stamp.clone()
                ],
            )
            .await?;

            tx.execute(
                "INSERT INTO activity (id, task_id, actor_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    task_id,
                    actor_id,
                    ActivityKind::Moved.as_str(),
                    format!("{from_name} to {to_name}"),
                    stamp.clone()
                ],
            )
            .await?;

            Ok::<_, turso::Error>(Outcome::Wrote)
        }
        .await;

        match written {
            Ok(Outcome::Wrote) => {
                tx.commit().await.map_err(backend)?;
                Ok(Moved::Recorded(Transition {
                    id: transition_id,
                    task_id: task_id.to_string(),
                    from_column: from_column_id.to_string(),
                    to_column: to_column_id.to_string(),
                    actor_id: actor_id.to_string(),
                    at,
                }))
            }
            Ok(Outcome::Stale) => {
                let _ = tx.rollback().await;
                Ok(Moved::Stale)
            }
            Ok(Outcome::Missing) => {
                let _ = tx.rollback().await;
                Err(StoreError::NotFound)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(backend(e))
            }
        }
    }

    async fn delete_task(
        &self,
        task_id: &str,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<Deletion> {
        let stamp = stamp(at)?;

        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: whether a task is freed depends on the edges that are
        // left after this one's are dropped. Reading them outside the write
        // would let a concurrent link make the answer wrong.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let written = async {
            let mut rows = tx
                .query(
                    "SELECT task_key, title, board_id FROM task \
                     WHERE id = ?1 AND deleted_at IS NULL",
                    params![task_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(None);
            };
            let task_key = row.get::<String>(0)?;
            let title = row.get::<String>(1)?;
            let board_id = row.get::<String>(2)?;
            drop(rows);

            // Everyone this task was standing in front of, before the edges go.
            let mut rows = tx
                .query(
                    "SELECT d.blocked_task_id FROM task_dependency d \
                     JOIN task t ON t.id = d.blocked_task_id \
                     WHERE d.blocking_task_id = ?1 AND d.cleared_at IS NULL \
                     AND t.deleted_at IS NULL",
                    params![task_id],
                )
                .await?;
            let mut waiting = Vec::new();
            while let Some(row) = rows.next().await? {
                waiting.push(row.get::<String>(0)?);
            }
            drop(rows);

            // The edges and the comments stay in the table: the delete is a
            // soft one, and every read filters on the task's deleted_at, so a
            // deleted task's links stop applying without being destroyed.
            tx.execute(
                "UPDATE task SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1",
                params![task_id, stamp.clone()],
            )
            .await?;
            tx.execute(
                "INSERT INTO activity (id, task_id, actor_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, ?4, '', ?5)",
                params![
                    Uuid::new_v4().to_string(),
                    task_id,
                    actor_id,
                    ActivityKind::Deleted.as_str(),
                    stamp.clone()
                ],
            )
            .await?;

            // Freed only if nothing else is still in front of them.
            let mut freed = Vec::new();
            for blocked in waiting {
                let mut rows = tx
                    .query(
                        "SELECT COUNT(*) FROM task_dependency d \
                         JOIN task t ON t.id = d.blocking_task_id \
                         WHERE d.blocked_task_id = ?1 AND d.cleared_at IS NULL \
                         AND t.deleted_at IS NULL",
                        params![blocked.clone()],
                    )
                    .await?;
                let left = match rows.next().await? {
                    Some(row) => row.get::<i64>(0)?,
                    None => 0,
                };
                drop(rows);
                if left > 0 {
                    continue;
                }
                tx.execute(
                    "INSERT INTO activity (id, task_id, actor_id, kind, detail, created_at) \
                     VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
                    params![
                        Uuid::new_v4().to_string(),
                        blocked.clone(),
                        ActivityKind::Unblocked.as_str(),
                        format!("{task_key} was deleted"),
                        stamp.clone()
                    ],
                )
                .await?;
                freed.push(blocked);
            }

            // The freeing is written in the same transaction as the delete, so
            // a mail owed because of it is owed by a fact that committed. A
            // delete that freed nobody is not an event: nothing can fire on it
            // and nothing needs to read it back.
            let event = if freed.is_empty() {
                None
            } else {
                let event = Freeing {
                    id: Uuid::new_v4().to_string(),
                    board_id,
                    cause_key: task_key.clone(),
                    cause_title: title,
                    actor_id: actor_id.to_string(),
                    at,
                };
                tx.execute(
                    "INSERT INTO freeing (id, board_id, cause_key, cause_title, actor_id, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        event.id.clone(),
                        event.board_id.clone(),
                        event.cause_key.clone(),
                        event.cause_title.clone(),
                        event.actor_id.clone(),
                        stamp.clone()
                    ],
                )
                .await?;
                Some(event)
            };
            Ok::<_, turso::Error>(Some(Deletion { freed, event }))
        }
        .await;

        match written {
            Ok(Some(deletion)) => {
                tx.commit().await.map_err(backend)?;
                Ok(deletion)
            }
            Ok(None) => {
                let _ = tx.rollback().await;
                Err(StoreError::NotFound)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(backend(e))
            }
        }
    }

    async fn deletion_cost(&self, task_id: &str) -> Result<Option<DeletionCost>> {
        let mut rows = self
            .conn
            .query(
                "SELECT task_key, title FROM task WHERE id = ?1 AND deleted_at IS NULL",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let Some(row) = rows.next().await.map_err(backend)? else {
            return Ok(None);
        };
        let task_key = text(&row, 0)?;
        let title = text(&row, 1)?;
        drop(rows);

        // Both counts in one sweep: the confirmation says what goes, and it
        // should not cost three round trips to say it.
        let mut rows = self
            .conn
            .query(
                "SELECT (SELECT COUNT(*) FROM comment WHERE task_id = ?1), \
                 (SELECT COUNT(*) FROM task_dependency d JOIN task t \
                  ON t.id = CASE WHEN d.blocked_task_id = ?1 \
                                 THEN d.blocking_task_id ELSE d.blocked_task_id END \
                  WHERE (d.blocked_task_id = ?1 OR d.blocking_task_id = ?1) \
                  AND d.cleared_at IS NULL AND t.deleted_at IS NULL)",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let (comment_count, link_count) = match rows.next().await.map_err(backend)? {
            Some(row) => (
                row.get::<i64>(0).map_err(backend)?.max(0) as u32,
                row.get::<i64>(1).map_err(backend)?.max(0) as u32,
            ),
            None => (0, 0),
        };
        drop(rows);

        // Who would be left with nothing in front of them. The same reading the
        // delete itself uses: an uncleared edge to a live task is what counts.
        let mut rows = self
            .conn
            .query(
                "SELECT t.task_key FROM task_dependency d \
                 JOIN task t ON t.id = d.blocked_task_id \
                 WHERE d.blocking_task_id = ?1 AND d.cleared_at IS NULL \
                 AND t.deleted_at IS NULL \
                 AND NOT EXISTS ( \
                   SELECT 1 FROM task_dependency o \
                   JOIN task b ON b.id = o.blocking_task_id \
                   WHERE o.blocked_task_id = t.id AND o.blocking_task_id <> ?1 \
                   AND o.cleared_at IS NULL AND b.deleted_at IS NULL)",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let mut frees = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            frees.push(text(&row, 0)?);
        }

        Ok(Some(DeletionCost {
            task_key,
            title,
            comment_count,
            link_count,
            frees,
        }))
    }

    async fn record_activity(
        &self,
        task_id: &str,
        actor_id: Option<&str>,
        kind: &ActivityKind,
        detail: &str,
        at: OffsetDateTime,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO activity (id, task_id, actor_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    task_id,
                    actor_id,
                    kind.as_str(),
                    detail,
                    stamp(at)?
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    // -- mail rules --------------------------------------------------------

    async fn create_mail_rule(
        &self,
        board_id: &str,
        trigger: &Trigger,
        subject: &str,
        audience: Audience,
        at: OffsetDateTime,
    ) -> Result<MailRule> {
        let id = Uuid::new_v4().to_string();
        let (kind, column) = trigger_parts(trigger);
        self.conn
            .execute(
                "INSERT INTO mail_rule \
                 (id, board_id, trigger_kind, trigger_column, subject, audience, enabled, \
                  created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
                params![
                    id.clone(),
                    board_id,
                    kind,
                    column,
                    subject,
                    audience_text(audience),
                    stamp(at)?
                ],
            )
            .await
            .map_err(backend)?;
        let sql = format!("SELECT {RULE_COLUMNS} FROM mail_rule WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => rule_from(&row),
            None => Err(StoreError::NotFound),
        }
    }

    async fn mail_rules(&self, board_id: &str) -> Result<Vec<MailRule>> {
        let sql = format!(
            "SELECT {RULE_COLUMNS} FROM mail_rule WHERE board_id = ?1 ORDER BY created_at, rowid"
        );
        let mut rows = self
            .conn
            .query(&sql, params![board_id])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(rule_from(&row)?);
        }
        Ok(out)
    }

    async fn update_mail_rule(
        &self,
        rule_id: &str,
        trigger: &Trigger,
        subject: &str,
        audience: Audience,
    ) -> Result<()> {
        let (kind, column) = trigger_parts(trigger);
        let n = self
            .conn
            .execute(
                "UPDATE mail_rule SET trigger_kind = ?1, trigger_column = ?2, subject = ?3, \
                 audience = ?4 WHERE id = ?5",
                params![kind, column, subject, audience_text(audience), rule_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn mail_rule(&self, rule_id: &str) -> Result<Option<MailRule>> {
        let sql = format!("SELECT {RULE_COLUMNS} FROM mail_rule WHERE id = ?1");
        match self.one_row(&sql, params![rule_id]).await? {
            Some(row) => rule_from(&row).map(Some),
            None => Ok(None),
        }
    }

    async fn board_of_task(&self, task_id: &str) -> Result<Option<String>> {
        match self
            .one_row("SELECT board_id FROM task WHERE id = ?1", params![task_id])
            .await?
        {
            Some(row) => Ok(Some(text(&row, 0)?)),
            None => Ok(None),
        }
    }

    async fn event(&self, event_id: &str) -> Result<Option<Event>> {
        if let Some(row) = self
            .one_row(
                "SELECT id, task_id, from_column, to_column, actor_id, created_at \
                 FROM transition WHERE id = ?1",
                params![event_id],
            )
            .await?
        {
            return Ok(Some(Event::Moved(Transition {
                id: text(&row, 0)?,
                task_id: text(&row, 1)?,
                from_column: text(&row, 2)?,
                to_column: text(&row, 3)?,
                actor_id: text(&row, 4)?,
                at: parse_stamp(&text(&row, 5)?)?,
            })));
        }
        match self
            .one_row(
                "SELECT id, board_id, cause_key, cause_title, actor_id, created_at \
                 FROM freeing WHERE id = ?1",
                params![event_id],
            )
            .await?
        {
            Some(row) => Ok(Some(Event::Freed(Freeing {
                id: text(&row, 0)?,
                board_id: text(&row, 1)?,
                cause_key: text(&row, 2)?,
                cause_title: text(&row, 3)?,
                actor_id: text(&row, 4)?,
                at: parse_stamp(&text(&row, 5)?)?,
            }))),
            None => Ok(None),
        }
    }

    async fn set_mail_rule_enabled(&self, rule_id: &str, enabled: bool) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE mail_rule SET enabled = ?1 WHERE id = ?2",
                params![i64::from(enabled), rule_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn delete_mail_rule(&self, rule_id: &str) -> Result<()> {
        // The ledger goes with the rule — ON DELETE CASCADE on `mail_send`,
        // which the foreign-keys pragma makes real.
        let n = self
            .conn
            .execute("DELETE FROM mail_rule WHERE id = ?1", params![rule_id])
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn mail_rule_last_sent(&self, board_id: &str) -> Result<Vec<(String, OffsetDateTime)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT s.rule_id, MAX(s.sent_at) FROM mail_send s \
                 JOIN mail_rule r ON r.id = s.rule_id \
                 WHERE r.board_id = ?1 AND s.sent_at IS NOT NULL \
                 GROUP BY s.rule_id",
                params![board_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push((text(&row, 0)?, parse_stamp(&text(&row, 1)?)?));
        }
        Ok(out)
    }

    // -- the send ledger ---------------------------------------------------

    async fn claim_send(
        &self,
        rule_id: &str,
        event_id: &str,
        task_id: &str,
        recipient: &str,
        at: OffsetDateTime,
    ) -> Result<Option<MailSend>> {
        let id = Uuid::new_v4().to_string();
        // The index is the decision. `DO NOTHING` turns the second engine run
        // into zero rows affected rather than an error to interpret, and the
        // caller that gets `None` sends nothing.
        let n = self
            .conn
            .execute(
                "INSERT INTO mail_send \
                 (id, rule_id, event_id, task_id, recipient, state, attempts, claimed_at, \
                  next_attempt_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6) \
                 ON CONFLICT (rule_id, event_id, task_id, recipient) DO NOTHING",
                params![
                    id.clone(),
                    rule_id,
                    event_id,
                    task_id,
                    recipient,
                    stamp(at)?
                ],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Ok(None);
        }
        let sql = format!("SELECT {SEND_COLUMNS} FROM mail_send WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => send_from(&row).map(Some),
            None => Err(StoreError::NotFound),
        }
    }

    async fn queue_invite(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
        at: OffsetDateTime,
    ) -> Result<MailSend> {
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO mail_send \
                 (id, recipient, state, attempts, claimed_at, next_attempt_at, kind, subject, \
                  body) \
                 VALUES (?1, ?2, 'pending', 0, ?3, ?3, 'invite', ?4, ?5)",
                params![id.clone(), recipient, stamp(at)?, subject, body],
            )
            .await
            .map_err(backend)?;
        let sql = format!("SELECT {SEND_COLUMNS} FROM mail_send WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => send_from(&row),
            None => Err(StoreError::NotFound),
        }
    }

    async fn record_send_accepted(&self, send_id: &str, at: OffsetDateTime) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE mail_send \
                 SET state = 'sent', attempts = attempts + 1, sent_at = ?1, \
                     next_attempt_at = NULL, last_error = NULL \
                 WHERE id = ?2",
                params![stamp(at)?, send_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn record_send_refused(
        &self,
        send_id: &str,
        error: &str,
        retry_at: Option<OffsetDateTime>,
        _at: OffsetDateTime,
    ) -> Result<()> {
        // A refusal is written whether or not it will be retried: a mail that
        // never arrived and left no trace is the failure that makes people stop
        // trusting the tool.
        let state = if retry_at.is_some() {
            "failed"
        } else {
            "abandoned"
        };
        let retry = match retry_at {
            Some(at) => Some(stamp(at)?),
            None => None,
        };
        let n = self
            .conn
            .execute(
                "UPDATE mail_send \
                 SET state = ?1, attempts = attempts + 1, last_error = ?2, next_attempt_at = ?3 \
                 WHERE id = ?4",
                params![state, error, retry, send_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn defer_send(
        &self,
        send_id: &str,
        reason: &str,
        retry_at: OffsetDateTime,
        _at: OffsetDateTime,
    ) -> Result<()> {
        // `attempts` is deliberately not touched. Everything else reads like a
        // refusal that will be retried, because that is what it is — the mail
        // is owed, it is due again shortly, and the reason says why it waited.
        let n = self
            .conn
            .execute(
                "UPDATE mail_send \
                 SET state = 'failed', last_error = ?1, next_attempt_at = ?2 \
                 WHERE id = ?3",
                params![reason, stamp(retry_at)?, send_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn sends_owed(&self, now: OffsetDateTime, limit: u32) -> Result<Vec<MailSend>> {
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send \
             WHERE next_attempt_at IS NOT NULL AND next_attempt_at <= ?1 \
             ORDER BY next_attempt_at LIMIT ?2"
        );
        let mut rows = self
            .conn
            .query(&sql, params![stamp(now)?, i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(send_from(&row)?);
        }
        Ok(out)
    }

    async fn sends_for_rule(&self, rule_id: &str, limit: u32) -> Result<Vec<MailSend>> {
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send WHERE rule_id = ?1 \
             ORDER BY claimed_at DESC, rowid DESC LIMIT ?2"
        );
        let mut rows = self
            .conn
            .query(&sql, params![rule_id, i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(send_from(&row)?);
        }
        Ok(out)
    }

    // -- mail decisions and observability -----------------------------------

    async fn record_mail_decision(
        &self,
        rule_id: &str,
        event_id: &str,
        task_id: &str,
        outcome: MailOutcome,
        detail: &str,
        at: OffsetDateTime,
    ) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO mail_decision (id, rule_id, event_id, task_id, outcome, detail, \
                 created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT (rule_id, event_id, task_id) DO NOTHING",
                params![id, rule_id, event_id, task_id, outcome.as_str(), detail, stamp(at)?],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn recent_mail_decisions(&self, limit: u32) -> Result<Vec<MailDecision>> {
        let sql = format!(
            "SELECT {DECISION_COLUMNS} FROM mail_decision \
             ORDER BY created_at DESC, rowid DESC LIMIT ?1"
        );
        let mut rows = self
            .conn
            .query(&sql, params![i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(decision_from(&row)?);
        }
        Ok(out)
    }

    async fn mail_rule_last_decision(&self) -> Result<Vec<(String, OffsetDateTime)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT rule_id, MAX(created_at) FROM mail_decision GROUP BY rule_id",
                (),
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push((text(&row, 0)?, parse_stamp(&text(&row, 1)?)?));
        }
        Ok(out)
    }

    async fn mail_queue(&self, limit: u32) -> Result<Vec<MailSend>> {
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send WHERE state IN ('pending', 'failed') \
             ORDER BY next_attempt_at LIMIT ?1"
        );
        let mut rows = self
            .conn
            .query(&sql, params![i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(send_from(&row)?);
        }
        Ok(out)
    }

    async fn recent_sends(&self, limit: u32) -> Result<Vec<MailSend>> {
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send ORDER BY claimed_at DESC, rowid DESC LIMIT ?1"
        );
        let mut rows = self
            .conn
            .query(&sql, params![i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(send_from(&row)?);
        }
        Ok(out)
    }

    async fn recent_activity(&self, limit: u32) -> Result<Vec<ActivityLine>> {
        let mut rows = self
            .conn
            .query(
                "SELECT a.task_id, t.title, u.display_name, a.kind, a.detail, a.created_at \
                 FROM activity a \
                 JOIN task t ON t.id = a.task_id \
                 LEFT JOIN user u ON u.id = a.actor_id \
                 ORDER BY a.created_at DESC, a.rowid DESC LIMIT ?1",
                params![i64::from(limit)],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(ActivityLine {
                task_id: text(&row, 0)?,
                title: text(&row, 1)?,
                actor_name: opt_text(&row, 2)?,
                kind: ActivityKind::parse(&text(&row, 3)?),
                detail: text(&row, 4)?,
                at: parse_stamp(&text(&row, 5)?)?,
            });
        }
        Ok(out)
    }

    // -- who gets mailed ---------------------------------------------------

    async fn recipients_for_task(&self, task_id: &str) -> Result<Vec<Recipient>> {
        // The role filter is belt as well as braces: a Viewer cannot be
        // assigned in the first place, and if one ever were, no mail would go
        // out to them from here.
        let mut rows = self
            .conn
            .query(
                "SELECT u.id, u.email, u.display_name FROM task_assignee a \
                 JOIN user u ON u.id = a.user_id \
                 WHERE a.task_id = ?1 AND u.role <> ?2 ORDER BY u.display_name",
                params![task_id, Role::Viewer.as_str()],
            )
            .await
            .map_err(backend)?;
        recipients_from(&mut rows).await
    }

    async fn recipients_for_board(&self, board_id: &str) -> Result<Vec<Recipient>> {
        let mut rows = self
            .conn
            .query(
                "SELECT u.id, u.email, u.display_name FROM user u \
                 JOIN board b ON b.workspace_id = u.workspace_id \
                 WHERE b.id = ?1 AND u.role <> ?2 ORDER BY u.display_name",
                params![board_id, Role::Viewer.as_str()],
            )
            .await
            .map_err(backend)?;
        recipients_from(&mut rows).await
    }

    async fn recipients_for_task_creator(&self, task_id: &str) -> Result<Vec<Recipient>> {
        let mut rows = self
            .conn
            .query(
                "SELECT u.id, u.email, u.display_name FROM task t \
                 JOIN user u ON u.id = t.created_by \
                 WHERE t.id = ?1 AND u.role <> ?2",
                params![task_id, Role::Viewer.as_str()],
            )
            .await
            .map_err(backend)?;
        recipients_from(&mut rows).await
    }
}

#[async_trait]
impl DetailReads for TursoStore {
    async fn task(&self, task_id: &str) -> Result<Option<TaskFacts>> {
        let row = self
            .one_row(
                "SELECT t.id, t.task_key, t.title, t.column_id, t.deadline, t.position, \
                 t.done_at, t.description, t.board_id, b.workspace_id \
                 FROM task t JOIN board b ON b.id = t.board_id \
                 WHERE t.id = ?1 AND t.deleted_at IS NULL",
                params![task_id],
            )
            .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(TaskFacts {
            row: TaskRow {
                id: text(&row, 0)?,
                task_key: text(&row, 1)?,
                title: text(&row, 2)?,
                column_id: text(&row, 3)?,
                deadline: opt_day(&row, 4)?,
                position: row.get::<f64>(5).map_err(backend)?,
                done_at: opt_stamp(&row, 6)?,
            },
            description: text(&row, 7)?,
            board_id: text(&row, 8)?,
            workspace_id: text(&row, 9)?,
        }))
    }

    async fn columns_for_board(&self, board_id: &str) -> Result<Vec<Column>> {
        BoardReads::columns(self, board_id).await
    }

    async fn assignees_for_task(&self, task_id: &str) -> Result<Vec<Person>> {
        let mut rows = self
            .conn
            .query(
                "SELECT u.id, u.display_name, u.photo_path FROM task_assignee a \
                 JOIN user u ON u.id = a.user_id \
                 WHERE a.task_id = ?1 ORDER BY u.display_name",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(Person {
                id: text(&row, 0)?,
                display_name: text(&row, 1)?,
                photo_path: opt_text(&row, 2)?,
            });
        }
        Ok(out)
    }

    async fn assignable_people(&self, workspace_id: &str) -> Result<Vec<Person>> {
        // Id, name and photo only: the picker shows no addresses and no roles,
        // so neither leaves the server for this screen.
        let mut rows = self
            .conn
            .query(
                "SELECT id, display_name, photo_path FROM user \
                 WHERE workspace_id = ?1 AND role <> ?2 ORDER BY display_name",
                params![workspace_id, Role::Viewer.as_str()],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(Person {
                id: text(&row, 0)?,
                display_name: text(&row, 1)?,
                photo_path: opt_text(&row, 2)?,
            });
        }
        Ok(out)
    }

    async fn dependencies_for_task(&self, task_id: &str) -> Result<Vec<(bool, DependencyEdge)>> {
        // Both directions in one round trip: the leading column says which.
        // `cleared_at` is only ever set by an unlink, so a cleared row is not a
        // link any more and does not belong on the screen. A link whose blocker
        // is finished still shows — that is `done_at`, and it reads as cleared.
        let mut rows = self
            .conn
            .query(
                "SELECT 1, t.id, t.task_key, t.title, d.cleared_at, t.done_at \
                 FROM task_dependency d JOIN task t ON t.id = d.blocking_task_id \
                 WHERE d.blocked_task_id = ?1 AND d.cleared_at IS NULL \
                 AND t.deleted_at IS NULL \
                 UNION ALL \
                 SELECT 0, t.id, t.task_key, t.title, d.cleared_at, t.done_at \
                 FROM task_dependency d JOIN task t ON t.id = d.blocked_task_id \
                 WHERE d.blocking_task_id = ?2 AND d.cleared_at IS NULL \
                 AND t.deleted_at IS NULL",
                params![task_id, task_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            let is_blocked_by = row.get::<i64>(0).map_err(backend)? != 0;
            out.push((
                is_blocked_by,
                DependencyEdge {
                    task_id: text(&row, 1)?,
                    task_key: text(&row, 2)?,
                    title: text(&row, 3)?,
                    cleared_at: opt_stamp(&row, 4)?,
                    done_at: opt_stamp(&row, 5)?,
                },
            ));
        }
        Ok(out)
    }

    async fn comments_for_task(&self, task_id: &str) -> Result<Vec<Comment>> {
        let mut rows = self
            .conn
            .query(
                "SELECT c.id, c.body, c.created_at, u.id, u.display_name, u.photo_path \
                 FROM comment c JOIN user u ON u.id = c.author_id \
                 WHERE c.task_id = ?1 ORDER BY c.created_at, c.rowid",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(Comment {
                id: text(&row, 0)?,
                body: text(&row, 1)?,
                at: parse_stamp(&text(&row, 2)?)?,
                author: Person {
                    id: text(&row, 3)?,
                    display_name: text(&row, 4)?,
                    photo_path: opt_text(&row, 5)?,
                },
            });
        }
        Ok(out)
    }

    async fn files_for_task(&self, task_id: &str) -> Result<Vec<FileLine>> {
        // `bytes` is not in the SELECT on purpose: a screen listing five files
        // must not drag five files through memory to print their names.
        let mut rows = self
            .conn
            .query(
                "SELECT id, file_name, size_bytes, comment_id, uploaded_by \
                 FROM attachment WHERE task_id = ?1 ORDER BY created_at, rowid",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(FileLine {
                id: text(&row, 0)?,
                name: text(&row, 1)?,
                size_bytes: row.get::<i64>(2).map_err(backend)?.max(0) as u64,
                comment_id: opt_text(&row, 3)?,
                uploaded_by: text(&row, 4)?,
            });
        }
        Ok(out)
    }

    async fn activity_for_task(&self, task_id: &str) -> Result<Vec<ActivityEntry>> {
        // LEFT JOIN: a line the system wrote has no actor, and dropping it
        // would hide exactly the events the rules engine causes.
        let mut rows = self
            .conn
            .query(
                "SELECT a.id, a.kind, a.detail, a.created_at, u.id, u.display_name, \
                 u.photo_path \
                 FROM activity a LEFT JOIN user u ON u.id = a.actor_id \
                 WHERE a.task_id = ?1 ORDER BY a.created_at, a.rowid",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            let actor = match opt_text(&row, 4)? {
                Some(id) => Some(Person {
                    id,
                    display_name: text(&row, 5)?,
                    photo_path: opt_text(&row, 6)?,
                }),
                None => None,
            };
            out.push(ActivityEntry {
                id: text(&row, 0)?,
                kind: ActivityKind::parse(&text(&row, 1)?),
                detail: text(&row, 2)?,
                at: parse_stamp(&text(&row, 3)?)?,
                actor,
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl BoardReads for TursoStore {
    async fn board(&self, workspace_id: &str) -> Result<Option<BoardMeta>> {
        let row = self
            .one_row(
                "SELECT id, name, task_prefix FROM board WHERE workspace_id = ?1 \
                 ORDER BY created_at LIMIT 1",
                params![workspace_id],
            )
            .await?;
        match row {
            Some(row) => Ok(Some(BoardMeta {
                id: text(&row, 0)?,
                name: text(&row, 1)?,
                task_prefix: text(&row, 2)?,
            })),
            None => Ok(None),
        }
    }

    async fn columns(&self, board_id: &str) -> Result<Vec<Column>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, name, position, is_done FROM board_column WHERE board_id = ?1 \
                 ORDER BY position",
                params![board_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(Column {
                id: text(&row, 0)?,
                name: text(&row, 1)?,
                position: row.get::<i64>(2).map_err(backend)?,
                is_done: row.get::<i64>(3).map_err(backend)? != 0,
            });
        }
        Ok(out)
    }

    async fn tasks_for_board(&self, board_id: &str) -> Result<Vec<TaskRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, task_key, title, column_id, deadline, position, done_at \
                 FROM task WHERE board_id = ?1 AND deleted_at IS NULL",
                params![board_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(TaskRow {
                id: text(&row, 0)?,
                task_key: text(&row, 1)?,
                title: text(&row, 2)?,
                column_id: text(&row, 3)?,
                deadline: opt_day(&row, 4)?,
                position: row.get::<f64>(5).map_err(backend)?,
                done_at: opt_stamp(&row, 6)?,
            });
        }
        Ok(out)
    }

    async fn assignees_for_board(&self, board_id: &str) -> Result<Vec<(String, Person)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT a.task_id, u.id, u.display_name, u.photo_path \
                 FROM task_assignee a \
                 JOIN task t ON t.id = a.task_id \
                 JOIN user u ON u.id = a.user_id \
                 WHERE t.board_id = ?1 AND t.deleted_at IS NULL \
                 ORDER BY u.display_name",
                params![board_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push((
                text(&row, 0)?,
                Person {
                    id: text(&row, 1)?,
                    display_name: text(&row, 2)?,
                    photo_path: opt_text(&row, 3)?,
                },
            ));
        }
        Ok(out)
    }

    async fn comment_counts_for_board(&self, board_id: &str) -> Result<Vec<(String, u32)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT c.task_id, COUNT(*) FROM comment c \
                 JOIN task t ON t.id = c.task_id \
                 WHERE t.board_id = ?1 AND t.deleted_at IS NULL \
                 GROUP BY c.task_id",
                params![board_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            let count = row.get::<i64>(1).map_err(backend)?.max(0) as u32;
            out.push((text(&row, 0)?, count));
        }
        Ok(out)
    }

    async fn dependencies_for_board(&self, board_id: &str) -> Result<Vec<(String, String)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT d.blocked_task_id, d.blocking_task_id FROM task_dependency d \
                 JOIN task t ON t.id = d.blocked_task_id \
                 WHERE t.board_id = ?1 AND d.cleared_at IS NULL AND t.deleted_at IS NULL",
                params![board_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push((text(&row, 0)?, text(&row, 1)?));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod probe {
    //! What Turso actually does about durability and concurrent writers.
    //!
    //! These are not assertions about our code; they record engine behaviour we
    //! are relying on. They fail loudly if a Turso upgrade changes it.

    use super::*;

    async fn pragma(conn: &Connection, name: &str) -> String {
        let mut rows = conn.query(&format!("PRAGMA {name}"), ()).await.unwrap();
        match rows.next().await.unwrap() {
            Some(row) => format!("{:?}", row.get_value(0).unwrap()),
            None => "<no row>".to_string(),
        }
    }

    #[tokio::test]
    async fn durability_defaults_are_recorded() {
        let dir = std::env::temp_dir().join(format!("izlek-probe-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.db");
        let db = Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        // WAL journalling with synchronous=FULL: a committed write is fsynced
        // before the commit returns. If a Turso upgrade weakens either of
        // these, this test is where we find out.
        assert_eq!(pragma(&conn, "journal_mode").await, "Text(\"wal\")");
        assert_eq!(pragma(&conn, "synchronous").await, "Integer(2)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_writers_on_one_database() {
        let dir = std::env::temp_dir().join(format!("izlek-probe-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.db");
        let db = Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let a = db.connect().unwrap();
        let b = db.connect().unwrap();
        a.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, who TEXT)", ())
            .await
            .unwrap();

        // Interleaved, both connections in flight at once.
        let write_a = async {
            for i in 0..50i64 {
                a.execute("INSERT INTO t (id, who) VALUES (?1, 'a')", params![i])
                    .await?;
            }
            Ok::<_, turso::Error>(())
        };
        let write_b = async {
            for i in 100..150i64 {
                b.execute("INSERT INTO t (id, who) VALUES (?1, 'b')", params![i])
                    .await?;
            }
            Ok::<_, turso::Error>(())
        };
        // Two connections on ONE handle: the engine serialises them for us.
        let (ra, rb) = tokio::join!(write_a, write_b);
        ra.expect("writer a");
        rb.expect("writer b");

        let c = db.connect().unwrap();
        let mut rows = c.query("SELECT COUNT(*) FROM t", ()).await.unwrap();
        let n = rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap();
        assert_eq!(n, 100, "no write lost between two connections");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_database_handles_on_one_file() {
        // Two handles on the same file is the shape a second process takes.
        let dir = std::env::temp_dir().join(format!("izlek-probe-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.db");
        let p = path.to_str().unwrap().to_string();

        let first = Builder::new_local(&p).build().await.unwrap();
        let setup = first.connect().unwrap();
        setup
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, who TEXT)", ())
            .await
            .unwrap();

        let second = Builder::new_local(&p).build().await;
        let second = match second {
            Ok(db) => db,
            Err(e) => {
                println!("second handle refused: {e}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };

        let a = setup;
        let b = second.connect().unwrap();
        for c in [&a, &b] {
            c.execute("PRAGMA busy_timeout = 5000", ()).await.unwrap();
        }
        let ha = tokio::spawn(async move {
            let mut errs = Vec::new();
            for i in 0..100i64 {
                if let Err(e) = a
                    .execute("INSERT INTO t (id, who) VALUES (?1, \'a\')", params![i])
                    .await
                {
                    errs.push(e.to_string());
                }
            }
            errs
        });
        let hb = tokio::spawn(async move {
            let mut errs = Vec::new();
            for i in 1000..1100i64 {
                if let Err(e) = b
                    .execute("INSERT INTO t (id, who) VALUES (?1, \'b\')", params![i])
                    .await
                {
                    errs.push(e.to_string());
                }
            }
            errs
        });
        let (ea, eb) = (ha.await.unwrap(), hb.await.unwrap());
        // With the busy timeout the store sets, a second handle waits its turn
        // instead of dropping writes. Without it, roughly 40% of these fail
        // with "database is locked".
        assert!(ea.is_empty(), "handle a: {ea:?}");
        assert!(eb.is_empty(), "handle b: {eb:?}");

        let third = Builder::new_local(&p).build().await.unwrap();
        let c = third.connect().unwrap();
        let mut rows = c.query("SELECT COUNT(*) FROM t", ()).await.unwrap();
        let n = rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap();
        assert_eq!(n, 200, "no write lost between two database handles");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The password has a column but never a way out. The table holds it,
    /// because the server must present it to the mail host on every send; the
    /// read path does not, because `WORKSPACE_COLUMNS` asks only whether one
    /// exists. This is the property the whole write-only field rests on, so it
    /// is asserted against the real query rather than against a comment.
    #[tokio::test]
    async fn the_workspace_read_path_cannot_carry_the_password() {
        let dir = std::env::temp_dir().join(format!("izlek-sender-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TursoStore::open(dir.join("izlek.db").to_str().unwrap())
            .await
            .unwrap();
        use crate::store::Store as _;

        let (ws, _admin) = store
            .claim_workspace("Izlek", "ada@izlek.sh", "Ada", "hash")
            .await
            .unwrap();
        store
            .set_sender(
                &ws.id,
                NewSender {
                    host: "smtp.fastmail.com".into(),
                    port: 587,
                    username: "izlek".into(),
                    password: Some("hunter2-and-then-some".into()),
                    from_name: "Izlek".into(),
                    from_address: "izlek@izlek.sh".into(),
                },
            )
            .await
            .unwrap();

        // The column exists and holds what was written — the mailer needs it.
        assert_eq!(
            store.smtp_password(&ws.id).await.unwrap().as_deref(),
            Some("hunter2-and-then-some")
        );

        // And the record a page would be handed does not contain it, anywhere,
        // under any field name.
        let loaded = store.workspace().await.unwrap().unwrap();
        assert!(loaded.smtp_password_set, "a password was stored");
        let serialised = serde_json::to_string(&loaded).unwrap();
        assert!(
            !serialised.contains("hunter2"),
            "the workspace record carried the password: {serialised}"
        );
        assert_eq!(loaded.smtp_host.as_deref(), Some("smtp.fastmail.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod migration {
    use super::*;

    /// A migration that dies halfway leaves nothing behind — not a table it
    /// created, and not a version row saying it ran. 0005 drops `mail_send`
    /// before it renames the rebuilt table into place, so a migration that can
    /// only half-apply is a migration that can lose the mail ledger.
    #[tokio::test]
    async fn a_migration_that_fails_partway_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("izlek-migration-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TursoStore::open(dir.join("izlek.db").to_str().unwrap())
            .await
            .unwrap();
        let before = store.schema_version().await.unwrap();

        let broken = "CREATE TABLE half_applied (id TEXT);\n\
                      INSERT INTO no_such_table (id) VALUES ('x');";
        assert!(
            store.apply(before + 1, broken).await.is_err(),
            "the second statement cannot succeed"
        );

        assert_eq!(
            store.schema_version().await.unwrap(),
            before,
            "a failed migration is not recorded as applied"
        );
        assert!(
            store
                .one_row(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'half_applied'",
                    (),
                )
                .await
                .unwrap()
                .is_none(),
            "and the table its first statement created is rolled back with it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
