//! Turso implementation of [`Store`].
//!
//! Turso is in-process and SQLite-compatible, so the schema is plain SQL and
//! there is no server to run alongside the binary. It ships no migration runner
//! of its own; [`TursoStore::open`] applies the numbered files in
//! `crates/izlek-core/migrations` at boot and records the version.

use async_trait::async_trait;
use rand::Rng;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, Row, Value, params, params_from_iter};
use ulid::Ulid;

use super::secret;
use super::{
    ActivityEvent, ActivityFilter, ActivityLine, Attachment, Audience, ClaimedSend, CommentWritten,
    Deletion, Dir, Event, FeedCursor, FeedPage, Freeing, LinkKind, MailDecision, MailOutcome,
    MailRule, MailSend, NewAttachment, NewSender, NewTask, NewUser, Recipient, Result, SendKind,
    SendState, SenderCheck, SenderTest, Session, SigninLink, Store, StoreError, Tag, TaskCreated,
    Trigger, User, UserStats, Workspace,
};
use super::{ReconcileOptions, reconcile, schema, sniff};
use crate::Role;
use crate::board::{BoardMeta, BoardReads, Column, Moved, Person, TagChip, TaskRow, Transition};
use crate::detail::{
    ActivityEntry, ActivityKind, Comment, DeletionCost, DependencyEdge, DetailReads, FileLine,
    SubtaskLine, TaskFacts, moment_label_in, parse_zone,
};
use crate::live::{Change, Topic};
use time::Date;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;

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
    conn: tokio::sync::Mutex<Connection>,
    db: turso::Database,
    /// Seals and opens `smtp_password`; see [`crate::store::secret`]. Never
    /// exposed through the `Store` trait — callers keep passing and receiving
    /// plaintext, this field is the detail that makes the row not be one.
    key: secret::Key,
    /// Where committed writes are announced. Held as the sender so the store
    /// can hand out a receiver per subscriber; nothing is ever read from here
    /// by the store itself.
    live: tokio::sync::broadcast::Sender<Change>,
    /// Root of the file tree the binary payloads live under: `attachments/`
    /// for task files, `photos/` for profile pictures, one raw file per row,
    /// named by the row's own id. The database keeps the facts and this tree
    /// keeps the bytes; a boot sweep deletes whichever half outlives the
    /// other.
    storage: std::path::PathBuf,
}

impl TursoStore {
    /// Announces a committed write. Called only once the write is durable —
    /// after commit for a transaction, and never on a path that returned an
    /// error — because a subscriber's whole job is to re-read, and waking it
    /// before the row lands makes it read the past.
    ///
    /// A send with no subscribers returns `Err`, which is the ordinary state
    /// of a server nobody is looking at. It is dropped on purpose: an
    /// announcement nobody is waiting for is not a problem, and logging it
    /// would fill the log with the sound of an idle app.
    fn announce(&self, topics: impl IntoIterator<Item = Topic>) {
        for topic in topics {
            let _ = self.live.send(Change {
                topic,
                seq: crate::live::next_seq(),
            });
        }
    }

    /// Opens (creating if needed) the database at `path` and brings the schema
    /// up to date. `storage` is the root of the file tree the binary payloads
    /// (attachment bytes, profile photos) live under — created here if
    /// missing. `:memory:` gives a throwaway database for tests, which still
    /// hand in a storage directory, usually a tempdir, for the files.
    pub async fn open(path: &str, storage: &std::path::Path) -> Result<Self> {
        // Before anything else, including the reconcile below that extracts
        // old blobs into this tree: a fresh storage path is a normal first
        // boot, and every writer past this point assumes the tree is there.
        ensure_storage_dirs(storage)?;
        let existed = path != ":memory:" && std::path::Path::new(path).exists();
        // Before anything holds this file open: a database of an older shape is
        // rebuilt now, while no handle of ours points at the file that the
        // rebuild is about to replace.
        Self::repair_if_stale(path, storage).await?;
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
        // 256 is deep enough that a client which pauses for a moment catches
        // up without noticing, and shallow enough that one wedged subscriber
        // cannot pin an unbounded backlog. Overflowing it is not a failure:
        // the reader is told it lagged and resyncs, which is cheaper than the
        // memory a larger buffer would cost to avoid saying so.
        let (live, _) = tokio::sync::broadcast::channel(256);
        let store = Self {
            conn: tokio::sync::Mutex::new(conn),
            db,
            key,
            live,
            storage: storage.to_path_buf(),
        };
        store.migrate(path).await?;
        store.encrypt_plaintext_passwords().await?;
        store.resniff_generic_attachments().await?;
        store.sweep_orphan_files().await?;
        // The file may have been rebuilt from a backup; its permissions and
        // any transient WAL/SHM siblings should still be private.
        if path != ":memory:" {
            restrict_if_present(std::path::Path::new(path))?;
            restrict_if_present(&sibling(path, "-wal"))?;
            restrict_if_present(&sibling(path, "-shm"))?;
        }
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
        let conn = self.conn.lock().await;
        let mut rows = conn
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
            conn.execute(
                "UPDATE workspace SET smtp_password = ?1 WHERE id = ?2",
                params![sealed, id],
            )
            .await
            .map_err(backend)?;
        }
        Ok(())
    }

    /// Re-decides the stored type of attachments whose `mime_type` is one of
    /// [`sniff::GENERIC_MIME_TYPES`] — rows written before the sniffer could
    /// name OOXML files, which a file chip then renders as a plain download
    /// because the viewer routes on the stored type alone. It runs here, once
    /// per boot, for the same reason the password-sealing pass above does: it
    /// is a data repair no migration can express, and a boot is the one moment
    /// the store may rewrite what uploads wrote. It is idempotent by
    /// construction — a row the pass refines leaves the generic buckets and is
    /// never read again, and a row the bytes cannot settle is left exactly as
    /// stored. Nothing is ever narrowed: the only verdicts written back are
    /// specific mime types that differ from what is stored.
    ///
    /// Cheap by design. The pass opens with a count of generic rows and, when
    /// there are none — every database after one pass, and every database
    /// whose uploads all postdate the sniffer — reads no file bytes at all.
    /// Otherwise each such row donates two windows, the first
    /// [`sniff::HEAD_WINDOW`] and last [`sniff::TAIL_WINDOW`] bytes, and only
    /// when the windows find a zip with no office marker in them does the row
    /// give up its central directory: the complete entry list, in which every
    /// entry name the file has appears, so a pptx or xlsx cannot hide its
    /// `ppt/presentation.xml` or `xl/workbook.xml` from the sweep. A window
    /// verdict is never trusted for `text/plain` — a window cannot prove a
    /// whole blob is UTF-8 — and a directory the end record locates beyond
    /// [`sniff::DIRECTORY_CAP`], or a zip64 one, is left unread and the row
    /// left alone.
    async fn resniff_generic_attachments(&self) -> Result<()> {
        let buckets = sniff::GENERIC_MIME_TYPES;
        let marks = (1..=buckets.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let conn = self.conn.lock().await;

        // The existence probe the whole pass hangs off: one count of a short
        // text column, no file reads, and the common case is done here.
        let mut rows = conn
            .query(
                &format!("SELECT COUNT(*) FROM attachment WHERE mime_type IN ({marks})"),
                params_from_iter(buckets.iter().copied()),
            )
            .await
            .map_err(backend)?;
        let count = match rows.next().await.map_err(backend)? {
            Some(row) => row.get::<i64>(0).map_err(backend)?,
            None => 0,
        };
        if count == 0 {
            return Ok(());
        }

        let mut rows = conn
            .query(
                &format!(
                    "SELECT id, mime_type FROM attachment WHERE mime_type IN ({marks})",
                    marks = marks,
                ),
                params_from_iter(buckets.iter().copied()),
            )
            .await
            .map_err(backend)?;
        let mut pending = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            let id = text(&row, 0)?;
            let stored = text(&row, 1)?;
            let path = attachment_file(&self.storage, &id);
            let (head, tail) = match read_windows(&path, sniff::HEAD_WINDOW, sniff::TAIL_WINDOW) {
                Ok(windows) => windows,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "attachment {id} wears a generic mime but its file is missing; \
                         leaving it as stored"
                    );
                    continue;
                }
                Err(e) => return Err(backend(e)),
            };
            let candidate = match sniff::refine(&stored, &head, &tail) {
                sniff::Verdict::Settle(candidate) => candidate,
                sniff::Verdict::ReadDirectory { offset, size } => {
                    let len = size.min(sniff::DIRECTORY_CAP as u64) as i64;
                    match read_window_at(&path, offset, len) {
                        Ok(directory) => sniff::office_entry_mime(&directory),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                        Err(e) => return Err(backend(e)),
                    }
                }
            };
            if let Some(mime) = sniff::refinement(&stored, candidate) {
                pending.push((mime, id));
            }
        }
        for (mime, id) in pending {
            conn.execute(
                "UPDATE attachment SET mime_type = ?1 WHERE id = ?2",
                params![mime, id],
            )
            .await
            .map_err(backend)?;
        }
        Ok(())
    }

    /// Sets the storage tree against the database, once per boot, after the
    /// resniff. The database and the tree are two halves of one state, and a
    /// crash between a row write and its file write — either order — leaves
    /// exactly one half behind. A file no row names goes, including a `.tmp`
    /// a crash abandoned mid-write; a row whose file is gone is only said
    /// out loud and otherwise kept, because deleting the row would turn a
    /// lost file into a lost fact — the row is still what screens list, and
    /// a re-upload replaces it cleanly.
    async fn sweep_orphan_files(&self) -> Result<()> {
        let conn = self.conn.lock().await;
        let attachments = known_ids(&conn, "SELECT id FROM attachment").await?;
        let photos = known_ids(&conn, "SELECT id FROM user WHERE photo_mime IS NOT NULL").await?;
        drop(conn);
        for (dir, known, kind) in [
            (
                self.storage.join(ATTACHMENTS_DIR),
                &attachments,
                "attachment",
            ),
            (self.storage.join(PHOTOS_DIR), &photos, "photo"),
        ] {
            for id in known.iter() {
                if !dir.join(id).is_file() {
                    eprintln!("{kind} {id} names a file that is not there");
                }
            }
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(backend(e)),
            };
            for entry in entries {
                let entry = entry.map_err(backend)?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let named = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| known.contains(n));
                if !named && let Err(e) = std::fs::remove_file(&path) {
                    eprintln!(
                        "could not delete orphaned storage file {}: {e}",
                        path.display()
                    );
                }
            }
        }
        Ok(())
    }

    /// Brings the database to the declared schema.
    ///
    /// - An empty database is created from `migrations/0001_init.sql`.
    /// - A database that already matches the declared schema is left alone.
    /// - A stale database is backed up, rebuilt, and re-verified once.
    ///   If the rebuilt database still does not match, the process stops
    ///   with the original backed up and untouched.
    ///
    /// SQLite's DDL is transactional, so a schema that dies halfway leaves
    /// nothing behind: a half-created database is a boot that starts over,
    /// not a database with a hole in it.
    async fn migrate(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                (),
            )
            .await
            .map_err(backend)?;

        let empty = match rows.next().await.map_err(backend)? {
            Some(row) => row.get::<i64>(0).map_err(backend)? == 0,
            None => true,
        };
        drop(rows);

        if empty {
            conn.execute("BEGIN IMMEDIATE", ()).await.map_err(backend)?;
            if let Err(e) = conn.execute_batch(super::schema::SCHEMA).await {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(backend(e));
            }
            return conn
                .execute("COMMIT", ())
                .await
                .map_err(backend)
                .map(|_| ());
        }

        if path == ":memory:" {
            // Tests own an in-memory database; its schema is whatever the
            // test created, and the open should not try to reconcile it.
            return Ok(());
        }

        // `repair_if_stale` ran before this handle was ever opened, so by now
        // the file on disk matches. Saying so here is cheap and turns a
        // reordering mistake into a refusal to start rather than a store
        // running against a schema the code does not expect.
        let have = schema::fingerprint(&conn).await.map_err(backend)?;
        let want = schema::declared_fingerprint().await.map_err(backend)?;
        if have != want {
            return Err(StoreError::Backend(format!(
                "database does not match the declared schema and was not repaired; \
                 do not restart. diff:\n{}",
                schema::diff_report(&have, &want)
            )));
        }
        Ok(())
    }

    /// Brings a database of an older shape onto the declared schema, BEFORE
    /// any long-lived handle is opened on it.
    ///
    /// The order matters and is the whole reason this is not part of
    /// `migrate`: `reconcile` swaps a rebuilt file into place, so a
    /// `turso::Database` opened beforehand still refers to the file that is
    /// now the backup. Reconnecting such a handle reads the old database and
    /// makes a successful rebuild look like a failed one.
    async fn repair_if_stale(path: &str, storage: &std::path::Path) -> Result<()> {
        if path == ":memory:" || !std::path::Path::new(path).exists() {
            return Ok(());
        }
        let (have, empty) = {
            let db = Builder::new_local(path).build().await.map_err(backend)?;
            let conn = db.connect().map_err(backend)?;
            let mut rows = conn
                .query(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                    (),
                )
                .await
                .map_err(backend)?;
            let empty = match rows.next().await.map_err(backend)? {
                Some(row) => row.get::<i64>(0).map_err(backend)? == 0,
                None => true,
            };
            drop(rows);
            let have = if empty {
                String::new()
            } else {
                schema::fingerprint(&conn).await.map_err(backend)?
            };
            (have, empty)
        };
        if empty {
            return Ok(());
        }
        let want = schema::declared_fingerprint().await.map_err(backend)?;
        if have == want {
            return Ok(());
        }

        eprintln!(
            "database schema differs from the declared schema; rebuilding automatically\n{}",
            schema::diff_report(&have, &want)
        );
        reconcile(
            path,
            Some(storage),
            ReconcileOptions {
                dry_run: false,
                yes: false,
                auto: true,
            },
        )
        .await?;

        // Check the result once, on a handle opened after the swap. A
        // normalisation bug that rebuilt forever would otherwise write a
        // full-size backup on every restart until the disk filled.
        let db = Builder::new_local(path).build().await.map_err(backend)?;
        let conn = db.connect().map_err(backend)?;
        let after = schema::fingerprint(&conn).await.map_err(backend)?;
        if after != want {
            return Err(StoreError::Backend(format!(
                "rebuild did not match the declared schema; the original is backed up beside it \
                 and this is not retried. diff:\n{}",
                schema::diff_report(&after, &want)
            )));
        }
        Ok(())
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
        let conn = self.conn.lock().await;
        let mut rows = conn.query(sql, args).await.map_err(backend)?;
        rows.next().await.map_err(backend)
    }

    /// Brings this task's reminder sends back in line with the task as it
    /// now stands, inside the caller's transaction. Every task write that
    /// can move a clock, finish a card or change who is on it calls this, so
    /// correctness never depends on the caller remembering which of the two
    /// facts moved.
    ///
    /// The rule is abandon first, then re-derive: every pending reminder the
    /// task still owes is abandoned unconditionally, and the task's current
    /// facts decide what is owed from scratch. A diff would be cleverer and
    /// wrong more often — a reminder row is a promise the queue is holding,
    /// and a promise whose grounds moved is re-made, not patched. Rows that
    /// already went out (or failed on their own retry clock) are history and
    /// stay untouched — and stay served: a recipient this task's reminders
    /// have already been written for is never minted a second row, or every
    /// later write about the card would quietly re-queue a mail they have
    /// already had, one copy per write.
    ///
    /// Answers whether anything was written or abandoned, so the caller can
    /// announce the queue only when this did something — the task-write
    /// paths already announce their own topics.
    async fn sync_task_reminders(
        &self,
        tx: &Transaction<'_>,
        task_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        // Who this task's reminders have already served: a row that went
        // out (`sent`), is riding its own retries (`failed`), or was given
        // up on by that same retry ladder (`abandoned` with attempts on the
        // clock) settles that recipient's reminder. A draft the re-derive
        // itself abandoned has attempts 0 and serves nobody — the promise it
        // stood for is unkept, and the mint below owes it fresh. Read before
        // the abandon below, in the same transaction: a row still pending
        // here is an unkept promise, and the moment it counts as served it
        // would be silenced forever.
        let mut rows = tx
            .query(
                "SELECT DISTINCT recipient FROM mail_send \
                 WHERE task_id = ?1 AND kind = 'reminder' \
                   AND (state IN ('sent', 'failed') \
                        OR (state = 'abandoned' AND attempts > 0))",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let mut served = std::collections::HashSet::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            served.insert(text(&row, 0)?);
        }
        drop(rows);

        // Nothing the re-derivation below can say changes a row that is no
        // longer pending.
        let abandoned = tx
            .execute(
                "UPDATE mail_send SET state = 'abandoned', next_attempt_at = NULL \
                 WHERE task_id = ?1 AND kind = 'reminder' AND state = 'pending'",
                params![task_id],
            )
            .await
            .map_err(backend)?;

        let mut rows = tx
            .query(
                "SELECT clock_at, done_at, deleted_at, task_key, title FROM task WHERE id = ?1",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let Some(row) = rows.next().await.map_err(backend)? else {
            return Ok(abandoned > 0);
        };
        let clock_at = opt_stamp(&row, 0)?;
        let done_at = opt_stamp(&row, 1)?;
        let deleted_at = opt_stamp(&row, 2)?;
        let task_key = text(&row, 3)?;
        let title = text(&row, 4)?;
        drop(rows);

        // A finished or discarded task owes nobody a warning, and a task
        // without a clock has nothing to warn about.
        let Some(clock_at) = clock_at else {
            return Ok(abandoned > 0);
        };
        if done_at.is_some() || deleted_at.is_some() || clock_at <= now {
            return Ok(abandoned > 0);
        }

        let mut rows = tx
            .query(
                "SELECT reminder_minutes, public_url FROM workspace LIMIT 1",
                (),
            )
            .await
            .map_err(backend)?;
        let Some(row) = rows.next().await.map_err(backend)? else {
            return Ok(abandoned > 0);
        };
        let reminder_minutes = row.get::<i64>(0).map_err(backend)?.max(0) as u32;
        let public_url = opt_text(&row, 1)?;
        drop(rows);
        if reminder_minutes == 0 {
            return Ok(abandoned > 0);
        }

        // The mail falls due this many minutes before the clock; a meeting
        // already inside the window warns the moment the write commits.
        let due = (clock_at - Duration::minutes(i64::from(reminder_minutes))).max(now);
        let minutes_left = (clock_at - now).whole_minutes();

        let mut rows = tx
            .query(
                "SELECT u.email, u.timezone FROM task_assignee a \
                 JOIN user u ON u.id = a.user_id WHERE a.task_id = ?1 \
                 ORDER BY u.email",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let mut people = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            people.push((text(&row, 0)?, text(&row, 1)?));
        }
        drop(rows);

        let mut minted = 0usize;
        for (email, timezone) in people {
            // Already reminded of this meeting, or their reminder is on the
            // queue's own retry clock: a second row here is a second mail.
            if served.contains(&email) {
                continue;
            }
            // Each recipient reads the moment in their own stored timezone:
            // the meeting is one fact, and what it says on the clock where
            // they sit is not.
            let moment = moment_label_in(clock_at, parse_zone(&timezone));
            let mut body = format!(
                "{key} — {title}\n\nMeets at {moment} — in {minutes_left} minutes.\n",
                key = task_key,
                title = title,
            );
            // Only the workspace's own address is used here. The config
            // fallback the mail engine applies lives outside the store, and
            // reading config from inside a transaction is not a trade worth
            // making: a workspace that never set its URL simply sends a
            // reminder without a link.
            if let Some(base) = &public_url {
                body.push_str(&format!("\n{base}/?task={task_id}\n"));
            }
            tx.execute(
                "INSERT INTO mail_send \
                 (id, task_id, recipient, state, attempts, claimed_at, next_attempt_at, kind, \
                  subject, body) \
                 VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5, 'reminder', ?6, ?7)",
                params![
                    Ulid::new().to_string(),
                    task_id,
                    email,
                    stamp(now)?,
                    stamp(due)?,
                    format!("Reminder: {title} ({task_key})"),
                    body,
                ],
            )
            .await
            .map_err(backend)?;
            minted += 1;
        }

        Ok(abandoned > 0 || minted > 0)
    }
}

const RULE_COLUMNS: &str = "id, board_id, trigger_kind, trigger_column, subject, audience, \
     enabled, created_at, include_task_details";

const SEND_COLUMNS: &str = "id, rule_id, event_id, task_id, recipient, state, attempts, \
     last_error, next_attempt_at, sent_at, kind, subject, body";

fn trigger_parts(trigger: &Trigger) -> (&'static str, Option<String>) {
    match trigger {
        // A status rule with no column is the every-column rule; the schema
        // allows the null, and nothing has to invent a second kind to say so.
        Trigger::StatusBecomes(column) => ("status", column.clone()),
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
        ("status", column) => Trigger::StatusBecomes(column),
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
        include_task_details: row.get::<i64>(8).map_err(backend)? != 0,
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
    let outcome = MailOutcome::parse(&raw)
        .ok_or_else(|| StoreError::Corrupt(format!("mail outcome {raw:?}")))?;
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

/// The storage tree's two subdirectories. One file per row under each, the
/// file named by the row's own id — an id the store minted, never anything
/// an upload carried, so a path here is always exactly one store-named file
/// deep and `file_name` stays a label.
const ATTACHMENTS_DIR: &str = "attachments";
const PHOTOS_DIR: &str = "photos";

/// Where attachment `id`'s bytes live.
fn attachment_file(storage: &std::path::Path, id: &str) -> std::path::PathBuf {
    storage.join(ATTACHMENTS_DIR).join(id)
}

/// Where `user_id`'s photo lives.
fn photo_file(storage: &std::path::Path, user_id: &str) -> std::path::PathBuf {
    storage.join(PHOTOS_DIR).join(user_id)
}

/// Creates the storage tree — the root and its two subdirectories — when it
/// is missing, private to the process owner. A fresh storage path is a
/// normal first boot, not a failure, and everything below this point
/// assumes the directories are there to write into.
fn ensure_storage_dirs(storage: &std::path::Path) -> Result<()> {
    for dir in [
        storage.to_path_buf(),
        storage.join(ATTACHMENTS_DIR),
        storage.join(PHOTOS_DIR),
    ] {
        std::fs::create_dir_all(&dir).map_err(|e| {
            StoreError::Backend(format!("creating storage directory {}: {e}", dir.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| StoreError::Backend(format!("restricting {}: {e}", dir.display())))?;
        }
    }
    Ok(())
}

/// Writes `bytes` to `path` through a temporary file in the same directory
/// and a rename: a crash or a full disk mid-write leaves whatever file was
/// there before intact, never a truncated one under the real name. The
/// temporary carries a name no row will ever wear, so one a crash abandoned
/// is exactly what the boot sweep deletes.
fn write_file_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!("{}.tmp", Ulid::new()));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Whatever of `buf.len()` bytes the file holds from here — a file that ends
/// (or shrinks) under a read hands over the short read, the same answer
/// `substr` gave, and the sniffer decides from what it gets or declines.
fn read_up_to(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read as _;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// `path`'s first `head` and last `tail` bytes — the windows the sniffer
/// decides from, the same shape `substr(bytes, ...)` used to hand over. A
/// file shorter than a window gives that window its whole self.
fn read_windows(
    path: &std::path::Path,
    head: i64,
    tail: i64,
) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    use std::io::Seek as _;
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut head_buf = vec![0u8; len.min(head.max(0) as u64) as usize];
    let got = read_up_to(&mut file, &mut head_buf)?;
    head_buf.truncate(got);
    file.seek(std::io::SeekFrom::Start(len - len.min(tail.max(0) as u64)))?;
    let mut tail_buf = vec![0u8; len.min(tail.max(0) as u64) as usize];
    let got = read_up_to(&mut file, &mut tail_buf)?;
    tail_buf.truncate(got);
    Ok((head_buf, tail_buf))
}

/// `len` bytes of `path` starting at `offset` — the central-directory read
/// `substr(bytes, offset + 1, len)` used to do. A directory the file does
/// not actually hold comes back short; the caller treats that as no
/// verdict, exactly as it treated `substr`'s empty answer.
fn read_window_at(path: &std::path::Path, offset: u64, len: i64) -> std::io::Result<Vec<u8>> {
    use std::io::Seek as _;
    let mut file = std::fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len.max(0) as usize];
    let got = read_up_to(&mut file, &mut buf)?;
    buf.truncate(got);
    Ok(buf)
}

/// Every id `sql` hands back — the set of files the sweep must keep.
async fn known_ids(conn: &Connection, sql: &str) -> Result<std::collections::HashSet<String>> {
    let mut rows = conn.query(sql, ()).await.map_err(backend)?;
    let mut out = std::collections::HashSet::new();
    while let Some(row) = rows.next().await.map_err(backend)? {
        out.insert(text(&row, 0)?);
    }
    Ok(out)
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
    Held,
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

/// The activity filter's AND clauses, as SQL text (leading `AND `, empty
/// when the filter matches everything) and the values it binds, in the order
/// the placeholders appear.
fn activity_filter_sql(filter: &ActivityFilter) -> Result<(String, Vec<Value>)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    match filter.actor.as_deref() {
        Some("system") => clauses.push("a.actor_id IS NULL".to_string()),
        Some(actor) => {
            clauses.push("a.actor_id = ?".to_string());
            vals.push(actor.to_string().into());
        }
        None => {}
    }
    if let Some(kind) = &filter.kind {
        clauses.push("a.kind = ?".to_string());
        vals.push(kind.clone().into());
    }
    if let Some(task_key) = &filter.task_key {
        clauses.push("t.task_key = ?".to_string());
        vals.push(task_key.clone().into());
    }
    if let Some((start, end)) = &filter.day {
        clauses.push("(a.created_at >= ? AND a.created_at < ?)".to_string());
        vals.push(stamp(*start)?.into());
        vals.push(stamp(*end)?.into());
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", clauses.join(" AND "))
    };
    Ok((sql, vals))
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
        mail_batch_minutes: row.get::<i64>(19).map_err(backend)?.max(0) as u32,
        reminder_minutes: row.get::<i64>(20).map_err(backend)?.max(0) as u32,
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
        sender_check: match opt_stamp(row, 16)? {
            Some(at) => Some(SenderCheck {
                at,
                took_ms: row.get::<i64>(17).map_err(backend)?.max(0) as u64,
                error: opt_text(row, 18)?,
            }),
            None => None,
        },
        public_url: opt_text(row, 15)?,
    })
}

// The password is not among these, and the last entry is why: the query asks
// the database whether a password exists and carries back a 0 or a 1. The value
// never leaves the row, so no caller can pass it on by accident.
const WORKSPACE_COLUMNS: &str = "id, name, created_at, attachment_limit_bytes, \
     allowed_file_types, photo_limit_bytes, smtp_host, smtp_port, smtp_username, \
     smtp_from_name, smtp_from_address, \
     (smtp_password IS NOT NULL AND smtp_password <> ''), \
     smtp_test_at, smtp_test_ms, smtp_test_error, public_url, \
     smtp_check_at, smtp_check_ms, smtp_check_error, mail_batch_minutes, reminder_minutes";

fn user_from(row: &Row) -> Result<User> {
    Ok(User {
        id: text(row, 0)?,
        workspace_id: text(row, 1)?,
        email: text(row, 2)?,
        display_name: text(row, 3)?,
        role: Role::parse(&text(row, 4)?).ok_or_else(|| StoreError::Corrupt("role".into()))?,
        password_hash: opt_text(row, 5)?,
        has_photo: row.get::<i64>(6).map_err(backend)? != 0,
        created_at: parse_stamp(&text(row, 7)?)?,
        last_signed_in_at: opt_stamp(row, 8)?,
        invited_by: opt_text(row, 9)?,
        timezone: text(row, 10)?,
        theme: text(row, 11)?,
        language: text(row, 12)?,
        ui: text(row, 13)?,
    })
}

const USER_COLUMNS: &str = "id, workspace_id, email, display_name, role, password_hash, \
     (photo_mime IS NOT NULL), created_at, last_signed_in_at, invited_by, timezone, theme, language, ui";

fn signin_link_from(row: &Row) -> Result<SigninLink> {
    Ok(SigninLink {
        id: text(row, 0)?,
        user_id: text(row, 1)?,
        created_at: parse_stamp(&text(row, 2)?)?,
        expires_at: parse_stamp(&text(row, 3)?)?,
        used_at: opt_stamp(row, 4)?,
        kind: LinkKind::from_str(&text(row, 5)?)
            .ok_or_else(|| StoreError::Corrupt("signin_link kind".into()))?,
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
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Change> {
        self.live.subscribe()
    }

    async fn claim_workspace(
        &self,
        workspace_name: &str,
        email: &str,
        display_name: &str,
        password_hash: &str,
    ) -> Result<(Workspace, User)> {
        let workspace_id = Ulid::new().to_string();
        let admin_id = Ulid::new().to_string();
        let board_id = Ulid::new().to_string();
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
                    now.clone(),
                ],
            )
            .await?;
            for (position, (name, is_done)) in DEFAULT_COLUMNS.iter().enumerate() {
                tx.execute(
                    "INSERT INTO board_column (id, board_id, name, position, is_done) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        Ulid::new().to_string(),
                        board_id.clone(),
                        *name,
                        position as i64,
                        i64::from(*is_done)
                    ],
                )
                .await?;
            }
            // Every task must wear a tag, so the board comes with the one
            // that catches whatever loses its own — English, like the
            // columns beside it.
            tx.execute(
                "INSERT INTO tag (id, board_id, name, position, is_default, created_at) \
                 VALUES (?1, ?2, 'General', 0, 1, ?3)",
                params![Ulid::new().to_string(), board_id.clone(), now.clone()],
            )
            .await?;
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
        // First write in the database's life: settings, members and the
        // board all begin here, so a subscriber on any of the three wakes.
        self.announce([Topic::Settings, Topic::Members, Topic::Board]);

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
        let conn = self.conn.lock().await;
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
        let sealed_password = sender
            .password
            .as_deref()
            .map(|p| secret::seal(&self.key, p));
        conn.execute(
            "UPDATE workspace SET smtp_host = ?1, smtp_port = ?2, smtp_username = ?3, \
                 smtp_password = COALESCE(?4, smtp_password), smtp_from_name = ?5, \
                 smtp_from_address = ?6, smtp_test_at = NULL, smtp_test_ms = NULL, \
                 smtp_test_error = NULL, smtp_check_at = NULL, smtp_check_ms = NULL, \
                 smtp_check_error = NULL WHERE id = ?7",
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
        drop(conn);
        self.announce([Topic::Settings]);
        Ok(())
    }

    async fn record_sender_check(&self, workspace_id: &str, check: SenderCheck) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE workspace SET smtp_check_at = ?1, smtp_check_ms = ?2, \
                 smtp_check_error = ?3 WHERE id = ?4",
            params![
                stamp(check.at)?,
                check.took_ms as i64,
                check.error,
                workspace_id
            ],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Settings]);
        Ok(())
    }

    async fn record_sender_test(&self, workspace_id: &str, test: SenderTest) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
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
        drop(conn);
        self.announce([Topic::Settings]);
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

    async fn set_public_url(&self, workspace_id: &str, public_url: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE workspace SET public_url = ?1 WHERE id = ?2",
            params![public_url, workspace_id],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Settings]);
        Ok(())
    }

    async fn set_limits(
        &self,
        workspace_id: &str,
        attachment_limit_bytes: u64,
        photo_limit_bytes: u64,
        allowed_file_types: &[String],
        mail_batch_minutes: u32,
        reminder_minutes: u32,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let types = serde_json::to_string(allowed_file_types)
            .map_err(|e| StoreError::Corrupt(format!("allowed_file_types: {e}")))?;
        conn.execute(
            "UPDATE workspace SET attachment_limit_bytes = ?1, photo_limit_bytes = ?2, \
                 allowed_file_types = ?3, mail_batch_minutes = ?5, reminder_minutes = ?6 \
                 WHERE id = ?4",
            params![
                attachment_limit_bytes as i64,
                photo_limit_bytes as i64,
                types,
                workspace_id,
                i64::from(mail_batch_minutes),
                i64::from(reminder_minutes)
            ],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Settings]);
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
        let id = Ulid::new().to_string();
        self.conn
            .lock()
            .await
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
            .map_err(|e| {
                if is_constraint_violation(&e) {
                    StoreError::Conflict("account")
                } else {
                    backend(e)
                }
            })?;
        self.announce([Topic::Members]);
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
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {USER_COLUMNS} FROM user WHERE workspace_id = ?1 ORDER BY created_at, id"
        );
        let mut rows = conn
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

    async fn user_stats(&self, user_id: &str) -> Result<UserStats> {
        // One row of four scalars: a profile reads its whole summary in a
        // single trip. `deleted_at` is excluded everywhere — a task off the
        // board is off the person's page as well.
        let row = self
            .one_row(
                "SELECT \
                   (SELECT COUNT(*) FROM task_assignee a \
                      JOIN task t ON t.id = a.task_id \
                      JOIN board_column c ON c.id = t.column_id \
                     WHERE a.user_id = ?1 AND t.deleted_at IS NULL AND c.is_done = 0), \
                   (SELECT COUNT(*) FROM task_assignee a \
                      JOIN task t ON t.id = a.task_id \
                      JOIN board_column c ON c.id = t.column_id \
                     WHERE a.user_id = ?1 AND t.deleted_at IS NULL AND c.is_done = 1), \
                   (SELECT COUNT(*) FROM task WHERE created_by = ?1 AND deleted_at IS NULL), \
                   (SELECT COUNT(*) FROM comment m JOIN task t ON t.id = m.task_id \
                     WHERE m.author_id = ?1 AND t.deleted_at IS NULL)",
                params![user_id],
            )
            .await?;
        let Some(row) = row else {
            return Ok(UserStats {
                assigned_open: 0,
                assigned_done: 0,
                created: 0,
                comments: 0,
            });
        };
        let at = |i: usize| -> Result<u32> {
            row.get::<i64>(i).map_err(backend).map(|n| n.max(0) as u32)
        };
        Ok(UserStats {
            assigned_open: at(0)?,
            assigned_done: at(1)?,
            created: at(2)?,
            comments: at(3)?,
        })
    }

    async fn set_password_hash(&self, user_id: &str, hash: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET password_hash = ?1 WHERE id = ?2",
                params![hash, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn set_profile(&self, user_id: &str, display_name: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET display_name = ?1 WHERE id = ?2",
                params![display_name, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn set_photo(&self, user_id: &str, bytes: &[u8], mime: &str) -> Result<()> {
        // The new bytes stage under a name no row wears, the row commits,
        // and only then does the rename put them over the old photo: a failed
        // update leaves the committed photo exactly as it was, and a crash
        // between commit and rename serves the old bytes next to a staged
        // file the boot sweep collects — stale, never destroyed.
        let path = photo_file(&self.storage, user_id);
        let staged = path.with_extension(format!("incoming-{}", Ulid::new()));
        write_file_atomic(&staged, bytes).map_err(|e| StoreError::Backend(e.to_string()))?;
        let conn = self.conn.lock().await;
        let written = conn
            .execute(
                "UPDATE user SET photo_mime = ?1 WHERE id = ?2",
                params![mime, user_id],
            )
            .await;
        let n = match written {
            Ok(n) => n,
            Err(e) => {
                drop(conn);
                let _ = std::fs::remove_file(&staged);
                return Err(backend(e));
            }
        };
        if n == 0 {
            drop(conn);
            let _ = std::fs::remove_file(&staged);
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            std::fs::rename(&staged, &path).map_err(|e| StoreError::Backend(e.to_string()))?;
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn clear_photo(&self, user_id: &str) -> Result<()> {
        // The row goes first: the file may only follow a delete that
        // committed, or a crash in between would leave a row whose photo is
        // gone. The unlink is best-effort — a file that survives it is
        // orphaned bytes the boot sweep collects.
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET photo_mime = NULL WHERE id = ?1",
                params![user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            let _ = std::fs::remove_file(photo_file(&self.storage, user_id));
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn photo(&self, user_id: &str) -> Result<Option<(Vec<u8>, String)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT photo_mime FROM user WHERE id = ?1",
                params![user_id],
            )
            .await
            .map_err(backend)?;
        let mime = match rows.next().await.map_err(backend)? {
            Some(row) => opt_text(&row, 0)?,
            None => return Ok(None),
        };
        drop(conn);
        let Some(mime) = mime else {
            return Ok(None);
        };
        match std::fs::read(photo_file(&self.storage, user_id)) {
            Ok(bytes) => Ok(Some((bytes, mime))),
            // A row whose file went missing is reported at boot; the photo
            // read answers "nothing to serve" rather than failing the page.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Backend(e.to_string())),
        }
    }

    async fn set_email(&self, user_id: &str, workspace_id: &str, email: &str) -> Result<()> {
        let email = fold_email(email);
        if let Some(existing) = self.user_by_email(workspace_id, &email).await?
            && existing.id != user_id
        {
            return Err(StoreError::Conflict("account"));
        }
        let conn = self.conn.lock().await;
        // The check above and this write are not one transaction: a second
        // claimant can slip in between them and take the unique index first,
        // so the write's own failure is read too, not just the pre-check's.
        let n = conn
            .execute(
                "UPDATE user SET email = ?1 WHERE id = ?2",
                params![email, user_id],
            )
            .await
            .map_err(|e| {
                if is_constraint_violation(&e) {
                    StoreError::Conflict("account")
                } else {
                    backend(e)
                }
            })?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn set_preferences(
        &self,
        user_id: &str,
        timezone: &str,
        theme: &str,
        language: &str,
        ui: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET timezone = ?1, theme = ?2, language = ?3, ui = ?4 WHERE id = ?5",
                params![timezone, theme, language, ui, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn set_role(&self, user_id: &str, role: Role) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET role = ?1 WHERE id = ?2",
                params![role.as_str(), user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn mark_signed_in(&self, user_id: &str, at: OffsetDateTime) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE user SET last_signed_in_at = ?1 WHERE id = ?2",
                params![stamp(at)?, user_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn create_signin_link(
        &self,
        user_id: &str,
        token_hash: &str,
        expires_at: OffsetDateTime,
        kind: LinkKind,
    ) -> Result<SigninLink> {
        let id = Ulid::new().to_string();
        self.conn
            .lock()
            .await
            .execute(
                "INSERT INTO signin_link (id, user_id, token_hash, created_at, expires_at, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id.clone(),
                    user_id,
                    token_hash,
                    now_text()?,
                    stamp(expires_at)?,
                    kind.as_str(),
                ],
            )
            .await
            .map_err(backend)?;
        self.announce([Topic::Members]);
        match self
            .one_row(
                "SELECT id, user_id, created_at, expires_at, used_at, kind \
                 FROM signin_link WHERE id = ?1",
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
                "SELECT id, user_id, created_at, expires_at, used_at, kind \
                 FROM signin_link WHERE token_hash = ?1",
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
        if n == 1 {
            self.announce([Topic::Members]);
        }
        Ok(n == 1)
    }

    async fn create_session(
        &self,
        user_id: &str,
        token_hash: &str,
        expires_at: OffsetDateTime,
    ) -> Result<Session> {
        let id = Ulid::new().to_string();
        self.conn
            .lock()
            .await
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
        self.announce([Topic::Members]);
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
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE session SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                params![stamp(at)?, id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            drop(conn);
            self.announce([Topic::Members]);
            Ok(())
        }
    }

    async fn revoke_sessions_for_user(&self, user_id: &str, at: OffsetDateTime) -> Result<u64> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE session SET revoked_at = ?1 WHERE user_id = ?2 AND revoked_at IS NULL",
                params![stamp(at)?, user_id],
            )
            .await
            .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Members]);
        Ok(n)
    }

    // The three auth-attempt methods below announce nothing, alone among the
    // writes here. They are rate-limit bookkeeping that no surface renders, and
    // announcing them would wake every connected client on every failed
    // sign-in — traffic in exchange for a screen that would look identical.
    async fn record_auth_attempt(&self, bucket: &str, at: OffsetDateTime) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO auth_attempt (id, bucket, attempted_at) VALUES (?1, ?2, ?3)",
            params![Ulid::new().to_string(), bucket, stamp(at)?],
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
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM auth_attempt WHERE bucket = ?1",
            params![bucket],
        )
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn prune_auth_attempts(&self, before: OffsetDateTime) -> Result<u64> {
        let conn = self.conn.lock().await;
        conn.execute(
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
        let conn = self.conn.lock().await;
        let id = Ulid::new().to_string();
        conn.execute(
            "INSERT INTO board_column (id, board_id, name, position, is_done) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id.clone(), board_id, name, position, i64::from(is_done)],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Board]);
        Ok(Column {
            id,
            name: name.to_string(),
            position,
            is_done,
        })
    }

    async fn create_task(&self, new: NewTask<'_>) -> Result<TaskCreated> {
        let id = Ulid::new().to_string();
        let activity_id = Ulid::new().to_string();
        let transition_id = Ulid::new().to_string();
        let at = OffsetDateTime::now_utc();
        let now = stamp(at)?;
        let deadline = new.deadline.map(day_text).transpose()?;
        let clock = new.clock_at.map(stamp).transpose()?;
        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: the task row, its activity and its transition land as one
        // write set; taking the write lock up front avoids a deferred-upgrade
        // deadlock against a concurrent writer.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let written = async {
            let mut rows = tx
                .query(
                    "SELECT task_prefix FROM board WHERE id = ?1",
                    params![new.board_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(Err(StoreError::NotFound));
            };
            let prefix = row.get::<String>(0)?;
            drop(rows);

            // A subtask is checked before anything is written: the parent has
            // to exist, sit on this board, and not be a subtask itself.
            if let Some(parent) = new.parent_id {
                let mut rows = tx
                    .query(
                        "SELECT board_id, parent_id FROM task \
                         WHERE id = ?1 AND deleted_at IS NULL",
                        params![parent],
                    )
                    .await?;
                let Some(row) = rows.next().await? else {
                    return Ok(Err(StoreError::NotFound));
                };
                let parent_board = row.get::<String>(0)?;
                let grandparent = row.get::<Option<String>>(1)?;
                drop(rows);
                if parent_board != new.board_id {
                    return Ok(Err(StoreError::OtherBoard));
                }
                if grandparent.is_some() {
                    return Ok(Err(StoreError::NotNestable));
                }
            }

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

            // A task always wears a tag, so one is chosen here when nobody
            // named one: the board's default. The NOT NULL on `task.tag_id`
            // makes choosing it the write's job, not an afterthought.
            let mut rows = tx
                .query(
                    "SELECT id FROM tag WHERE board_id = ?1 AND is_default = 1",
                    params![new.board_id],
                )
                .await?;
            let default_tag = match rows.next().await? {
                Some(row) => row.get::<String>(0)?,
                None => return Ok(Err(StoreError::NotFound)),
            };
            drop(rows);
            // The key's tail is the end of this task's own id (a ULID —
            // Crockford-uppercase already, and the last chars are its random
            // bits) rather than a per-board counter: a counter leaves visible
            // gaps once tasks are deleted. Collisions are rare enough (the
            // tail is random) that a short bounded retry with a longer tail
            // is simpler than reserving ranges.
            let mut tail_len = 5;
            let task_key = loop {
                let candidate = format!("{prefix}-{}", &id[id.len() - tail_len..]);
                match tx
                    .execute(
                        "INSERT INTO task (id, board_id, parent_id, task_key, title, description, \
                         column_id, tag_id, deadline, clock_at, position, created_by, created_at, \
                         updated_at) \
                         VALUES (?1, ?2, ?11, ?3, ?4, ?5, ?6, ?12, ?7, ?13, ?8, ?9, ?10, ?10)",
                        params![
                            id.clone(),
                            new.board_id,
                            candidate.clone(),
                            new.title,
                            new.description,
                            new.column_id,
                            deadline.clone(),
                            position,
                            new.created_by,
                            now.clone(),
                            new.parent_id,
                            default_tag.clone(),
                            clock.clone()
                        ],
                    )
                    .await
                {
                    Ok(_) => break candidate,
                    Err(e) if is_constraint_violation(&e) && tail_len < 7 => {
                        tail_len += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            };
            tx.execute(
                "INSERT INTO activity (id, task_id, actor_id, subject_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, '', ?5)",
                params![
                    activity_id.clone(),
                    id.clone(),
                    new.created_by,
                    ActivityKind::Created.as_str(),
                    now.clone()
                ],
            )
            .await?;
            tx.execute(
                "INSERT INTO transition \
                 (id, task_id, from_column, to_column, actor_id, created_at) \
                 VALUES (?1, ?2, '', ?3, ?4, ?5)",
                params![
                    transition_id.clone(),
                    id.clone(),
                    new.column_id,
                    new.created_by,
                    now.clone()
                ],
            )
            .await?;
            Ok::<_, turso::Error>(Ok((task_key, position)))
        }
        .await;
        let written = match written {
            Ok(Ok(written)) => written,
            Ok(Err(refused)) => {
                let _ = tx.rollback().await;
                return Err(refused);
            }
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(backend(e));
            }
        };
        // The reminder rows are part of the same fact as the task itself: a
        // crash between the two would leave a clock nobody is reminded of.
        // Anything short of success rolls the whole write back.
        let reminded = match self.sync_task_reminders(&tx, &id, at).await {
            Ok(reminded) => reminded,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(e);
            }
        };
        tx.commit().await.map_err(backend)?;

        // Task birth touches the board and the task page at once.
        let mut topics = vec![Topic::Board, Topic::Task(id.clone()), Topic::Activity];
        if reminded {
            topics.push(Topic::Queue);
        }
        self.announce(topics);
        let (task_key, position) = written;
        Ok(TaskCreated {
            row: TaskRow {
                id: id.clone(),
                task_key,
                title: new.title.to_string(),
                column_id: new.column_id.to_string(),
                deadline: new.deadline,
                clock_at: new.clock_at,
                position,
                done_at: None,
                parent_id: new.parent_id.map(str::to_string),
                tag: None,
            },
            activity_id,
            transition: Transition {
                id: transition_id,
                task_id: id.clone(),
                from_column: String::new(),
                to_column: new.column_id.to_string(),
                actor_id: new.created_by.to_string(),
                at,
            },
        })
    }

    async fn set_parent(&self, task_id: &str, parent_id: Option<&str>) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: the one-level rule is read-then-write. Two concurrent
        // parentings each read a NULL parent, each pass, and the pair leaves a
        // grandchild neither of them could see.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let outcome = async {
            let mut rows = tx
                .query(
                    "SELECT board_id FROM task WHERE id = ?1 AND deleted_at IS NULL",
                    params![task_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(Err(StoreError::NotFound));
            };
            let board_id = row.get::<String>(0)?;
            drop(rows);

            if let Some(parent) = parent_id {
                if parent == task_id {
                    return Ok(Err(StoreError::Cycle));
                }
                let mut rows = tx
                    .query(
                        "SELECT board_id, parent_id FROM task \
                         WHERE id = ?1 AND deleted_at IS NULL",
                        params![parent],
                    )
                    .await?;
                let Some(row) = rows.next().await? else {
                    return Ok(Err(StoreError::NotFound));
                };
                let parent_board = row.get::<String>(0)?;
                let grandparent = row.get::<Option<String>>(1)?;
                drop(rows);
                if parent_board != board_id {
                    return Ok(Err(StoreError::OtherBoard));
                }
                if grandparent.is_some() {
                    return Ok(Err(StoreError::NotNestable));
                }

                // The other half of the same rule: a task that already has
                // subtasks cannot become one, or its own children become
                // grandchildren without ever being touched.
                let mut rows = tx
                    .query(
                        "SELECT 1 FROM task WHERE parent_id = ?1 AND deleted_at IS NULL LIMIT 1",
                        params![task_id],
                    )
                    .await?;
                let has_children = rows.next().await?.is_some();
                drop(rows);
                if has_children {
                    return Ok(Err(StoreError::NotNestable));
                }
            }

            tx.execute(
                "UPDATE task SET parent_id = ?2 WHERE id = ?1",
                params![task_id, parent_id],
            )
            .await?;
            Ok::<_, turso::Error>(Ok(()))
        }
        .await;

        match outcome {
            Ok(Ok(())) => {
                tx.commit().await.map_err(backend)?;
                self.announce([Topic::Board, Topic::Task(task_id.to_string())]);
                Ok(())
            }
            Ok(Err(refused)) => {
                let _ = tx.rollback().await;
                Err(refused)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(backend(e))
            }
        }
    }

    async fn subtasks(&self, parent_id: &str) -> Result<Vec<TaskRow>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT id, task_key, title, column_id, deadline, clock_at, position, done_at \
                 FROM task WHERE parent_id = ?1 AND deleted_at IS NULL \
                 ORDER BY created_at",
                params![parent_id],
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
                clock_at: opt_stamp(&row, 5)?,
                position: row.get::<f64>(6).unwrap_or(0.0),
                done_at: opt_stamp(&row, 7)?,
                parent_id: Some(parent_id.to_string()),
                tag: None,
            });
        }
        Ok(out)
    }

    async fn assign_task(&self, task_id: &str, user_id: &str) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: the reminder sync reads the assignee list this insert is
        // changing, so the read and the write are one write set.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let n = tx
            .execute(
                "INSERT OR IGNORE INTO task_assignee (task_id, user_id) VALUES (?1, ?2)",
                params![task_id, user_id],
            )
            .await
            .map_err(backend)?;
        // A no-change assign changes no facts, so the reminders stand as
        // they are.
        let reminded = if n > 0 {
            match self
                .sync_task_reminders(&tx, task_id, OffsetDateTime::now_utc())
                .await
            {
                Ok(reminded) => reminded,
                Err(e) => {
                    let _ = tx.rollback().await;
                    return Err(e);
                }
            }
        } else {
            false
        };
        tx.commit().await.map_err(backend)?;
        if n > 0 {
            let mut topics = vec![Topic::Task(task_id.to_string())];
            if reminded {
                topics.push(Topic::Queue);
            }
            self.announce(topics);
        }
        Ok(())
    }

    async fn unassign_task(&self, task_id: &str, user_id: &str) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        // IMMEDIATE: the same shape as the assign — the person leaves and the
        // reminder that named them goes with them, as one write.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let n = tx
            .execute(
                "DELETE FROM task_assignee WHERE task_id = ?1 AND user_id = ?2",
                params![task_id, user_id],
            )
            .await
            .map_err(backend)?;
        let reminded = if n > 0 {
            match self
                .sync_task_reminders(&tx, task_id, OffsetDateTime::now_utc())
                .await
            {
                Ok(reminded) => reminded,
                Err(e) => {
                    let _ = tx.rollback().await;
                    return Err(e);
                }
            }
        } else {
            false
        };
        tx.commit().await.map_err(backend)?;
        if n > 0 {
            let mut topics = vec![Topic::Task(task_id.to_string())];
            if reminded {
                topics.push(Topic::Queue);
            }
            self.announce(topics);
        }
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

        // A parent and its own part are already related, and the board would
        // draw the relationship twice: once as a chip, once as the count.
        {
            let conn = self.conn.lock().await;
            let mut rows = conn
                .query(
                    "SELECT 1 FROM task WHERE (id = ?1 AND parent_id = ?2) \
                     OR (id = ?2 AND parent_id = ?1) LIMIT 1",
                    params![blocked_task_id, blocking_task_id],
                )
                .await
                .map_err(backend)?;
            let related = rows.next().await.map_err(backend)?.is_some();
            if related {
                return Err(StoreError::Cycle);
            }
        }

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
            Ok(true) => {
                tx.commit().await.map_err(backend)?;
                // An edge is visible from both endpoints and on the board.
                self.announce([
                    Topic::Board,
                    Topic::Task(blocked_task_id.to_string()),
                    Topic::Task(blocking_task_id.to_string()),
                ]);
                Ok(())
            }
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
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE task_dependency SET cleared_at = ?3 \
                 WHERE blocked_task_id = ?1 AND blocking_task_id = ?2 AND cleared_at IS NULL",
                params![blocked_task_id, blocking_task_id, stamp(at)?],
            )
            .await
            .map_err(backend)?;
        drop(conn);
        if n > 0 {
            self.announce([
                Topic::Board,
                Topic::Task(blocked_task_id.to_string()),
                Topic::Task(blocking_task_id.to_string()),
            ]);
        }
        Ok(())
    }

    async fn add_comment(
        &self,
        task_id: &str,
        author_id: &str,
        body: &str,
        at: OffsetDateTime,
    ) -> Result<CommentWritten> {
        let comment_id = Ulid::new().to_string();
        let activity_id = Ulid::new().to_string();
        let when = stamp(at)?;

        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        let written = async {
            tx.execute(
                "INSERT INTO comment (id, task_id, author_id, body, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![comment_id.clone(), task_id, author_id, body, when.clone()],
            )
            .await?;
            tx.execute(
                "INSERT INTO activity (id, task_id, actor_id, subject_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, '', ?5)",
                params![
                    activity_id.clone(),
                    task_id,
                    author_id,
                    ActivityKind::Commented.as_str(),
                    when.clone()
                ],
            )
            .await?;
            Ok::<_, turso::Error>(())
        }
        .await;

        if let Err(e) = written {
            let _ = tx.rollback().await;
            return Err(backend(e));
        }
        tx.commit().await.map_err(backend)?;

        self.announce([Topic::Task(task_id.to_string()), Topic::Activity]);
        Ok(CommentWritten {
            comment_id,
            activity_id,
        })
    }

    async fn add_attachment(&self, new: NewAttachment<'_>) -> Result<String> {
        // The file lands first, temp-plus-rename, then the row says it is
        // there: a crash in between leaves an orphan file the boot sweep
        // deletes, never a row pointing at nothing. If the row write fails,
        // the file is unlinked best-effort — the row was never born, so the
        // bytes must not outlive it under a name nothing points at.
        let id = Ulid::new().to_string();
        let size = new.bytes.len() as i64;
        let path = attachment_file(&self.storage, &id);
        write_file_atomic(&path, &new.bytes).map_err(|e| StoreError::Backend(e.to_string()))?;
        let conn = self.conn.lock().await;
        let written = conn
            .execute(
                "INSERT INTO attachment (id, task_id, comment_id, file_name, mime_type, \
                     size_bytes, uploaded_by, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id.clone(),
                    new.task_id,
                    new.comment_id,
                    new.file_name,
                    new.mime_type,
                    size,
                    new.uploaded_by,
                    stamp(new.at)?
                ],
            )
            .await;
        if let Err(e) = written {
            drop(conn);
            let _ = std::fs::remove_file(&path);
            return Err(backend(e));
        }
        drop(conn);
        self.announce([Topic::Task(new.task_id.to_string())]);
        Ok(id)
    }

    async fn attachments(&self, task_id: &str) -> Result<Vec<Attachment>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
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
        let conn = self.conn.lock().await;
        let mut rows = conn
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
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT 1 FROM attachment WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        let known = rows.next().await.map_err(backend)?.is_some();
        drop(conn);
        if !known {
            return Ok(None);
        }
        match std::fs::read(attachment_file(&self.storage, id)) {
            Ok(bytes) => Ok(Some(bytes)),
            // A row whose file went missing is reported at boot; the read
            // answers "nothing to serve" rather than failing the panel.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Backend(e.to_string())),
        }
    }

    async fn delete_attachment(&self, id: &str) -> Result<bool> {
        // Which task loses the file has to be read before the row goes: after
        // the delete there is nothing left to ask, and a detail panel that is
        // never told still shows the attachment.
        let owner = self
            .one_row("SELECT task_id FROM attachment WHERE id = ?1", params![id])
            .await?
            .map(|row| row.get::<String>(0).map_err(backend))
            .transpose()?;
        let conn = self.conn.lock().await;
        let gone = conn
            .execute("DELETE FROM attachment WHERE id = ?1", params![id])
            .await
            .map_err(backend)?;
        drop(conn);
        if gone > 0 {
            // After the delete: the file may only follow a delete that
            // committed, or a crash in between would leave a row whose file
            // is gone. Best-effort — a file that survives is orphaned bytes
            // the boot sweep collects.
            let _ = std::fs::remove_file(attachment_file(&self.storage, id));
            if let Some(task_id) = owner {
                self.announce([Topic::Task(task_id)]);
            }
        }
        Ok(gone > 0)
    }

    async fn save_task(
        &self,
        task_id: &str,
        title: &str,
        description: &str,
        deadline: Option<Date>,
        clock_at: Option<OffsetDateTime>,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<Vec<String>> {
        let deadline = deadline.map(day_text).transpose()?;
        let clock_at = clock_at.map(stamp).transpose()?;
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
                    "SELECT title, description, deadline, clock_at FROM task \
                     WHERE id = ?1 AND deleted_at IS NULL",
                    params![task_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(None);
            };
            let was_title = row.get::<String>(0)?;
            let was_description = row.get::<String>(1)?;
            let was_deadline = row.get::<Option<String>>(2)?;
            let was_clock = row.get::<Option<String>>(3)?;
            drop(rows);

            tx.execute(
                "UPDATE task SET title = ?2, description = ?3, deadline = ?4, clock_at = ?5, \
                 updated_at = ?6 \
                 WHERE id = ?1",
                params![
                    task_id,
                    title,
                    description,
                    deadline.clone(),
                    clock_at.clone(),
                    stamp.clone()
                ],
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
            // The clock's detail is the stored stamp, as the deadline's is
            // the stored day: the activity row keeps the machine fact, and
            // each reader says it in its own words.
            if was_clock != clock_at {
                match &clock_at {
                    Some(when) => lines.push((ActivityKind::ClockSet.as_str(), when.clone())),
                    None => lines.push((ActivityKind::ClockCleared.as_str(), String::new())),
                }
            }
            let mut ids = Vec::with_capacity(lines.len());
            for (kind, detail) in lines {
                let id = Ulid::new().to_string();
                tx.execute(
                    "INSERT INTO activity (id, task_id, actor_id, subject_id, kind, detail, created_at) \
                     VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6)",
                    params![id.clone(), task_id, actor_id, kind, detail, stamp.clone()],
                )
                .await?;
                ids.push(id);
            }
            Ok::<_, turso::Error>(Some(ids))
        }
        .await;

        match written {
            Ok(Some(ids)) => {
                // The clock may just have moved, so the reminders are
                // re-derived inside the same write and the queue hears about
                // it only if rows actually changed hands.
                let reminded = match self.sync_task_reminders(&tx, task_id, at).await {
                    Ok(reminded) => reminded,
                    Err(e) => {
                        let _ = tx.rollback().await;
                        return Err(e);
                    }
                };
                tx.commit().await.map_err(backend)?;
                // An edit shows on the card face as well as in the panel.
                let mut topics = vec![
                    Topic::Board,
                    Topic::Task(task_id.to_string()),
                    Topic::Activity,
                ];
                if reminded {
                    topics.push(Topic::Queue);
                }
                self.announce(topics);
                Ok(ids)
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

    async fn move_task(
        &self,
        task_id: &str,
        from_column_id: &str,
        to_column_id: &str,
        actor_id: &str,
        at: OffsetDateTime,
    ) -> Result<Moved> {
        let stamp = stamp(at)?;
        let transition_id = Ulid::new().to_string();

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

            // A parent does not finish before its parts do. Read inside the
            // same transaction as the move: outside it, a subtask created
            // while the drag was in the air would slip under the check.
            if to_is_done {
                let mut rows = tx
                    .query(
                        "SELECT 1 FROM task \
                         WHERE parent_id = ?1 AND deleted_at IS NULL AND done_at IS NULL \
                         LIMIT 1",
                        params![task_id],
                    )
                    .await?;
                let open_child = rows.next().await?.is_some();
                drop(rows);
                if open_child {
                    return Ok(Outcome::Held);
                }
            }

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
                "INSERT INTO activity (id, task_id, actor_id, subject_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6)",
                params![
                    Ulid::new().to_string(),
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
                // The move is what stamps and clears `done_at` — finishing
                // and reopening a card are moves — so the reminders follow
                // the same write: finishing abandons them, reopening
                // re-derives them.
                let reminded = match self.sync_task_reminders(&tx, task_id, at).await {
                    Ok(reminded) => reminded,
                    Err(e) => {
                        let _ = tx.rollback().await;
                        return Err(e);
                    }
                };
                tx.commit().await.map_err(backend)?;
                // Past the commit, so a woken client re-reads the move rather
                // than the state before it. The rolled-back arms below say
                // nothing on purpose: nothing changed to re-read.
                let mut topics = vec![
                    Topic::Board,
                    Topic::Task(task_id.to_string()),
                    Topic::Activity,
                ];
                if reminded {
                    topics.push(Topic::Queue);
                }
                self.announce(topics);
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
            Ok(Outcome::Held) => {
                let _ = tx.rollback().await;
                Ok(Moved::Held)
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
            // The parts go with the whole, in the same write. A subtask that
            // outlived its parent would be unreachable: the board does not
            // show it, and the page that did is gone.
            tx.execute(
                "UPDATE task SET deleted_at = ?2, updated_at = ?2 \
                 WHERE parent_id = ?1 AND deleted_at IS NULL",
                params![task_id, stamp.clone()],
            )
            .await?;
            let deleted_activity_id = Ulid::new().to_string();
            tx.execute(
                "INSERT INTO activity (id, task_id, actor_id, subject_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, '', ?5)",
                params![
                    deleted_activity_id.clone(),
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
                    "INSERT INTO activity (id, task_id, actor_id, subject_id, kind, detail, created_at) \
                     VALUES (?1, ?2, NULL, NULL, ?3, ?4, ?5)",
                    params![
                        Ulid::new().to_string(),
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
                    id: Ulid::new().to_string(),
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
            Ok::<_, turso::Error>(Some(Deletion {
                freed,
                event,
                activity_id: deleted_activity_id,
            }))
        }
        .await;

        match written {
            Ok(Some(deletion)) => {
                // A deleted task's reminders die with it, in the same write
                // that deletes the task.
                let reminded = match self.sync_task_reminders(&tx, task_id, at).await {
                    Ok(reminded) => reminded,
                    Err(e) => {
                        let _ = tx.rollback().await;
                        return Err(e);
                    }
                };
                tx.commit().await.map_err(backend)?;
                // The card left the board and its detail panel is now a page
                // about nothing — both need to hear, and only after commit.
                let mut topics = vec![
                    Topic::Board,
                    Topic::Task(task_id.to_string()),
                    Topic::Activity,
                ];
                if reminded {
                    topics.push(Topic::Queue);
                }
                self.announce(topics);
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
        let conn = self.conn.lock().await;
        let mut rows = conn
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
        let mut rows = conn
            .query(
                "SELECT (SELECT COUNT(*) FROM comment WHERE task_id = ?1), \
                 (SELECT COUNT(*) FROM task_dependency d JOIN task t \
                  ON t.id = CASE WHEN d.blocked_task_id = ?1 \
                                 THEN d.blocking_task_id ELSE d.blocked_task_id END \
                  WHERE (d.blocked_task_id = ?1 OR d.blocking_task_id = ?1) \
                  AND d.cleared_at IS NULL AND t.deleted_at IS NULL), \
                 (SELECT COUNT(*) FROM task \
                  WHERE parent_id = ?1 AND deleted_at IS NULL)",
                params![task_id],
            )
            .await
            .map_err(backend)?;
        let (comment_count, link_count, subtask_count) = match rows.next().await.map_err(backend)? {
            Some(row) => (
                row.get::<i64>(0).map_err(backend)?.max(0) as u32,
                row.get::<i64>(1).map_err(backend)?.max(0) as u32,
                row.get::<i64>(2).map_err(backend)?.max(0) as u32,
            ),
            None => (0, 0, 0),
        };
        drop(rows);

        // Who would be left with nothing in front of them. The same reading the
        // delete itself uses: an uncleared edge to a live task is what counts.
        let mut rows = conn
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
            subtask_count,
        }))
    }

    async fn record_activity(
        &self,
        task_id: &str,
        actor_id: Option<&str>,
        subject_id: Option<&str>,
        kind: &ActivityKind,
        detail: &str,
        at: OffsetDateTime,
    ) -> Result<String> {
        let conn = self.conn.lock().await;
        let id = Ulid::new().to_string();
        conn.execute(
            "INSERT INTO activity (id, task_id, actor_id, subject_id, kind, detail, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id.clone(),
                task_id,
                actor_id,
                subject_id,
                kind.as_str(),
                detail,
                stamp(at)?
            ],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        // Task-scoped: the task's own trail and the workspace feed both grew.
        self.announce([Topic::Activity, Topic::Task(task_id.to_string())]);
        Ok(id)
    }

    async fn record_event(
        &self,
        actor_id: Option<&str>,
        kind: &ActivityKind,
        detail: &str,
        at: OffsetDateTime,
    ) -> Result<String> {
        let conn = self.conn.lock().await;
        let id = Ulid::new().to_string();
        conn.execute(
            "INSERT INTO activity (id, task_id, actor_id, subject_id, kind, detail, created_at) \
                 VALUES (?1, NULL, ?2, NULL, ?3, ?4, ?5)",
            params![id.clone(), actor_id, kind.as_str(), detail, stamp(at)?],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        // Workspace-wide, belonging to no task — the feed only.
        self.announce([Topic::Activity]);
        Ok(id)
    }

    // -- mail rules --------------------------------------------------------

    async fn create_mail_rule(
        &self,
        board_id: &str,
        trigger: &Trigger,
        subject: &str,
        audience: Audience,
        at: OffsetDateTime,
        include_task_details: bool,
    ) -> Result<MailRule> {
        let id = Ulid::new().to_string();
        let (kind, column) = trigger_parts(trigger);
        self.conn
            .lock()
            .await
            .execute(
                "INSERT INTO mail_rule \
                 (id, board_id, trigger_kind, trigger_column, subject, audience, enabled, \
                  created_at, include_task_details) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)",
                params![
                    id.clone(),
                    board_id,
                    kind,
                    column,
                    subject,
                    audience_text(audience),
                    stamp(at)?,
                    i64::from(include_task_details)
                ],
            )
            .await
            .map_err(backend)?;
        self.announce([Topic::Rules]);
        let sql = format!("SELECT {RULE_COLUMNS} FROM mail_rule WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => rule_from(&row),
            None => Err(StoreError::NotFound),
        }
    }

    async fn mail_rules(&self, board_id: &str) -> Result<Vec<MailRule>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {RULE_COLUMNS} FROM mail_rule WHERE board_id = ?1 ORDER BY created_at, rowid"
        );
        let mut rows = conn.query(&sql, params![board_id]).await.map_err(backend)?;
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
        include_task_details: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let (kind, column) = trigger_parts(trigger);
        let n = conn
            .execute(
                "UPDATE mail_rule SET trigger_kind = ?1, trigger_column = ?2, subject = ?3, \
                 audience = ?4, include_task_details = ?5 WHERE id = ?6",
                params![
                    kind,
                    column,
                    subject,
                    audience_text(audience),
                    i64::from(include_task_details),
                    rule_id
                ],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        drop(conn);
        self.announce([Topic::Rules]);
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
            Some(row) => {
                return Ok(Some(Event::Freed(Freeing {
                    id: text(&row, 0)?,
                    board_id: text(&row, 1)?,
                    cause_key: text(&row, 2)?,
                    cause_title: text(&row, 3)?,
                    actor_id: text(&row, 4)?,
                    at: parse_stamp(&text(&row, 5)?)?,
                })));
            }
            None => {}
        }
        match self
            .one_row(
                "SELECT activity.id, activity.task_id, task.board_id, activity.kind, \
                 activity.actor_id, activity.subject_id, activity.detail, activity.created_at \
                 FROM activity JOIN task ON task.id = activity.task_id \
                 WHERE activity.id = ?1",
                params![event_id],
            )
            .await?
        {
            Some(row) => Ok(Some(Event::Happened(ActivityEvent {
                id: text(&row, 0)?,
                task_id: text(&row, 1)?,
                board_id: text(&row, 2)?,
                kind: ActivityKind::parse(&text(&row, 3)?),
                actor_id: opt_text(&row, 4)?.unwrap_or_default(),
                subject_id: opt_text(&row, 5)?,
                detail: text(&row, 6)?,
                at: parse_stamp(&text(&row, 7)?)?,
            }))),
            None => Ok(None),
        }
    }

    async fn set_mail_rule_enabled(&self, rule_id: &str, enabled: bool) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE mail_rule SET enabled = ?1 WHERE id = ?2",
                params![i64::from(enabled), rule_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        drop(conn);
        self.announce([Topic::Rules]);
        Ok(())
    }

    async fn delete_mail_rule(&self, rule_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        // The ledger goes with the rule — ON DELETE CASCADE on `mail_send`,
        // which the foreign-keys pragma makes real.
        let n = conn
            .execute("DELETE FROM mail_rule WHERE id = ?1", params![rule_id])
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        drop(conn);
        // The rule's queued mail cascaded away with it, so the queue moved too.
        self.announce([Topic::Rules, Topic::Queue]);
        Ok(())
    }

    async fn mail_rule_last_sent(&self, board_id: &str) -> Result<Vec<(String, OffsetDateTime)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
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
        until: OffsetDateTime,
    ) -> Result<Option<ClaimedSend>> {
        let id = Ulid::new().to_string();
        // The index is the decision. `DO NOTHING` turns the second engine run
        // into zero rows affected rather than an error to interpret, and the
        // caller that gets `None` sends nothing.
        let n = self
            .conn
            .lock()
            .await
            .execute(
                "INSERT INTO mail_send \
                 (id, rule_id, event_id, task_id, recipient, state, attempts, claimed_at, \
                  next_attempt_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?7) \
                 ON CONFLICT (rule_id, event_id, task_id, recipient) DO NOTHING",
                params![
                    id.clone(),
                    rule_id,
                    event_id,
                    task_id,
                    recipient,
                    stamp(at)?,
                    stamp(until)?
                ],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Ok(None);
        }
        // A mail joined the queue, so the queue panel is out of date.
        self.announce([Topic::Queue]);
        let sql = format!("SELECT {SEND_COLUMNS} FROM mail_send WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => send_from(&row).map(ClaimedSend::taken).map(Some),
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
        let id = Ulid::new().to_string();
        self.conn
            .lock()
            .await
            .execute(
                "INSERT INTO mail_send \
                 (id, recipient, state, attempts, claimed_at, next_attempt_at, kind, subject, \
                  body) \
                 VALUES (?1, ?2, 'pending', 0, ?3, ?3, 'invite', ?4, ?5)",
                params![id.clone(), recipient, stamp(at)?, subject, body],
            )
            .await
            .map_err(backend)?;
        // Two surfaces at once: the invite is a queued mail, and an invited
        // person shows up in the members list before they ever sign in.
        self.announce([Topic::Queue, Topic::Members]);
        let sql = format!("SELECT {SEND_COLUMNS} FROM mail_send WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => send_from(&row),
            None => Err(StoreError::NotFound),
        }
    }

    async fn queue_notice(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
        at: OffsetDateTime,
    ) -> Result<MailSend> {
        let id = Ulid::new().to_string();
        self.conn
            .lock()
            .await
            .execute(
                "INSERT INTO mail_send \
                 (id, recipient, state, attempts, claimed_at, next_attempt_at, kind, subject, \
                  body) \
                 VALUES (?1, ?2, 'pending', 0, ?3, ?3, 'notice', ?4, ?5)",
                params![id.clone(), recipient, stamp(at)?, subject, body],
            )
            .await
            .map_err(backend)?;
        self.announce([Topic::Queue]);
        let sql = format!("SELECT {SEND_COLUMNS} FROM mail_send WHERE id = ?1");
        match self.one_row(&sql, params![id]).await? {
            Some(row) => send_from(&row),
            None => Err(StoreError::NotFound),
        }
    }

    async fn record_send_accepted(&self, send_id: &str, at: OffsetDateTime) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
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
        // The connection guard goes first: announcing is a send on a channel
        // whose subscribers may turn round and read this store, and holding
        // the single connection while they do is how that deadlocks.
        drop(conn);
        self.announce([Topic::Queue]);
        Ok(())
    }

    async fn record_send_refused(
        &self,
        send_id: &str,
        error: &str,
        retry_at: Option<OffsetDateTime>,
        _at: OffsetDateTime,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
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
        let n = conn
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
        drop(conn);
        // The attempt counter and the next-try time both just moved, and the
        // queue panel renders both.
        self.announce([Topic::Queue]);
        Ok(())
    }

    async fn defer_send(
        &self,
        send_id: &str,
        reason: &str,
        retry_at: OffsetDateTime,
        _at: OffsetDateTime,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        // `attempts` is deliberately not touched. Everything else reads like a
        // refusal that will be retried, because that is what it is — the mail
        // is owed, it is due again shortly, and the reason says why it waited.
        let n = conn
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
        drop(conn);
        self.announce([Topic::Queue]);
        Ok(())
    }

    async fn next_due_at(&self) -> Result<Option<OffsetDateTime>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT MIN(next_attempt_at) FROM mail_send WHERE next_attempt_at IS NOT NULL",
                (),
            )
            .await
            .map_err(backend)?;
        let Some(row) = rows.next().await.map_err(backend)? else {
            return Ok(None);
        };
        // MIN over no rows is one row holding NULL, not an empty result.
        match row.get_value(0).map_err(backend)? {
            Value::Text(at) => parse_stamp(&at).map(Some),
            _ => Ok(None),
        }
    }

    async fn claim_sends_owed(
        &self,
        now: OffsetDateTime,
        until: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<ClaimedSend>> {
        // One lock across the read and every claim: no second pass is inside
        // this while it runs, and every path into this store goes through the
        // same connection.
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send \
             WHERE next_attempt_at IS NOT NULL AND next_attempt_at <= ?1 \
             ORDER BY next_attempt_at LIMIT ?2"
        );
        let mut rows = conn
            .query(&sql, params![stamp(now)?, i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut due = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            due.push(send_from(&row)?);
        }

        let mut taken = Vec::new();
        for send in due {
            // The `next_attempt_at <= ?2` is the whole of the exclusion: a
            // pass that gets here after this row was leased finds it pushed
            // out of the window and changes nothing, so it is not returned to
            // that pass either.
            let moved = conn
                .execute(
                    "UPDATE mail_send SET next_attempt_at = ?1 \
                     WHERE id = ?3 AND next_attempt_at IS NOT NULL AND next_attempt_at <= ?2",
                    params![stamp(until)?, stamp(now)?, send.id.as_str()],
                )
                .await
                .map_err(backend)?;
            if moved == 1 {
                taken.push(ClaimedSend::taken(send));
            }
        }
        Ok(taken)
    }

    async fn sends_owed(&self, now: OffsetDateTime, limit: u32) -> Result<Vec<MailSend>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send \
             WHERE next_attempt_at IS NOT NULL AND next_attempt_at <= ?1 \
             ORDER BY next_attempt_at LIMIT ?2"
        );
        let mut rows = conn
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
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send WHERE rule_id = ?1 \
             ORDER BY claimed_at DESC, rowid DESC LIMIT ?2"
        );
        let mut rows = conn
            .query(&sql, params![rule_id, i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(send_from(&row)?);
        }
        Ok(out)
    }

    async fn sends_for_task(&self, task_id: &str, limit: u32) -> Result<Vec<MailSend>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send WHERE task_id = ?1 \
             ORDER BY claimed_at DESC, rowid DESC LIMIT ?2"
        );
        let mut rows = conn
            .query(&sql, params![task_id, i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(send_from(&row)?);
        }
        Ok(out)
    }

    async fn requeue_send(&self, send_id: &str, at: OffsetDateTime) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE mail_send SET state = 'pending', next_attempt_at = ?1 \
                 WHERE id = ?2 AND state IN ('failed', 'abandoned')",
            params![stamp(at)?, send_id],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        self.announce([Topic::Queue]);
        Ok(())
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
        let conn = self.conn.lock().await;
        let id = Ulid::new().to_string();
        conn.execute(
            "INSERT INTO mail_decision (id, rule_id, event_id, task_id, outcome, detail, \
                 created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT (rule_id, event_id, task_id) DO NOTHING",
            params![
                id,
                rule_id,
                event_id,
                task_id,
                outcome.as_str(),
                detail,
                stamp(at)?
            ],
        )
        .await
        .map_err(backend)?;
        drop(conn);
        // A decision is what the rules page's decisions tab renders.
        self.announce([Topic::Rules]);
        Ok(())
    }

    async fn recent_mail_decisions(&self, limit: u32, page: FeedPage) -> Result<Vec<MailDecision>> {
        let conn = self.conn.lock().await;
        let reverse = matches!(page, FeedPage::After(_));
        let mut rows = match &page {
            FeedPage::Newest => {
                let sql = format!(
                    "SELECT {DECISION_COLUMNS} FROM mail_decision \
                     ORDER BY created_at DESC, id DESC LIMIT ?1"
                );
                conn.query(&sql, params![i64::from(limit)]).await
            }
            FeedPage::Before(cursor) => {
                let sql = format!(
                    "SELECT {DECISION_COLUMNS} FROM mail_decision \
                     WHERE created_at < ?2 OR (created_at = ?2 AND id < ?3) \
                     ORDER BY created_at DESC, id DESC LIMIT ?1"
                );
                conn.query(
                    &sql,
                    params![i64::from(limit), stamp(cursor.at)?, cursor.id.clone()],
                )
                .await
            }
            FeedPage::After(cursor) => {
                let sql = format!(
                    "SELECT {DECISION_COLUMNS} FROM mail_decision \
                     WHERE created_at > ?2 OR (created_at = ?2 AND id > ?3) \
                     ORDER BY created_at ASC, id ASC LIMIT ?1"
                );
                conn.query(
                    &sql,
                    params![i64::from(limit), stamp(cursor.at)?, cursor.id.clone()],
                )
                .await
            }
        }
        .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(decision_from(&row)?);
        }
        if reverse {
            out.reverse();
        }
        Ok(out)
    }

    async fn decisions_for_task(&self, task_id: &str, limit: u32) -> Result<Vec<MailDecision>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {DECISION_COLUMNS} FROM mail_decision WHERE task_id = ?1 \
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        );
        let mut rows = conn
            .query(&sql, params![task_id, i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(decision_from(&row)?);
        }
        Ok(out)
    }

    async fn count_mail_decisions(&self) -> Result<u64> {
        let row = self
            .one_row("SELECT COUNT(*) FROM mail_decision", ())
            .await?;
        row.as_ref().map(count_of).unwrap_or(Ok(0))
    }

    async fn count_mail_decisions_preceding(&self, cursor: Option<&FeedCursor>) -> Result<u64> {
        let Some(cursor) = cursor else { return Ok(0) };
        let row = self
            .one_row(
                "SELECT COUNT(*) FROM mail_decision \
                 WHERE created_at > ?1 OR (created_at = ?1 AND id > ?2)",
                params![stamp(cursor.at)?, cursor.id.clone()],
            )
            .await?;
        row.as_ref().map(count_of).unwrap_or(Ok(0))
    }

    async fn mail_rule_last_decision(&self) -> Result<Vec<(String, OffsetDateTime)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
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

    async fn mail_queue(&self, limit: u32, page: FeedPage) -> Result<Vec<MailSend>> {
        let conn = self.conn.lock().await;
        let reverse = matches!(page, FeedPage::After(_));
        let mut rows = match &page {
            FeedPage::Newest => {
                let sql = format!(
                    "SELECT {SEND_COLUMNS} FROM mail_send WHERE state IN ('pending', 'failed') \
                     ORDER BY next_attempt_at ASC, id ASC LIMIT ?1"
                );
                conn.query(&sql, params![i64::from(limit)]).await
            }
            // Older: strictly later than the cursor, same ascending order the
            // queue reads in. `next_attempt_at` only ever moves forward on a
            // retry, so a row already shown here can never slip past this
            // boundary backward — it can only reappear ahead of it, later.
            FeedPage::Before(cursor) => {
                let sql = format!(
                    "SELECT {SEND_COLUMNS} FROM mail_send WHERE state IN ('pending', 'failed') \
                     AND (next_attempt_at > ?2 OR (next_attempt_at = ?2 AND id > ?3)) \
                     ORDER BY next_attempt_at ASC, id ASC LIMIT ?1"
                );
                conn.query(
                    &sql,
                    params![i64::from(limit), stamp(cursor.at)?, cursor.id.clone()],
                )
                .await
            }
            // Newer: strictly earlier, scanned backward then reversed so the
            // page still renders soonest-first.
            FeedPage::After(cursor) => {
                let sql = format!(
                    "SELECT {SEND_COLUMNS} FROM mail_send WHERE state IN ('pending', 'failed') \
                     AND (next_attempt_at < ?2 OR (next_attempt_at = ?2 AND id < ?3)) \
                     ORDER BY next_attempt_at DESC, id DESC LIMIT ?1"
                );
                conn.query(
                    &sql,
                    params![i64::from(limit), stamp(cursor.at)?, cursor.id.clone()],
                )
                .await
            }
        }
        .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(send_from(&row)?);
        }
        if reverse {
            out.reverse();
        }
        Ok(out)
    }

    async fn count_mail_queue(&self) -> Result<u64> {
        let row = self
            .one_row(
                "SELECT COUNT(*) FROM mail_send WHERE state IN ('pending', 'failed')",
                (),
            )
            .await?;
        row.as_ref().map(count_of).unwrap_or(Ok(0))
    }

    async fn count_mail_queue_preceding(&self, cursor: Option<&FeedCursor>) -> Result<u64> {
        let Some(cursor) = cursor else { return Ok(0) };
        let row = self
            .one_row(
                "SELECT COUNT(*) FROM mail_send WHERE state IN ('pending', 'failed') \
                 AND (next_attempt_at < ?1 OR (next_attempt_at = ?1 AND id < ?2))",
                params![stamp(cursor.at)?, cursor.id.clone()],
            )
            .await?;
        row.as_ref().map(count_of).unwrap_or(Ok(0))
    }

    async fn recent_sends(&self, limit: u32) -> Result<Vec<MailSend>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {SEND_COLUMNS} FROM mail_send ORDER BY claimed_at DESC, rowid DESC LIMIT ?1"
        );
        let mut rows = conn
            .query(&sql, params![i64::from(limit)])
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(send_from(&row)?);
        }
        Ok(out)
    }

    async fn recent_activity(
        &self,
        limit: u32,
        page: FeedPage,
        dir: Dir,
        filter: &ActivityFilter,
    ) -> Result<Vec<ActivityLine>> {
        const SELECT: &str = "SELECT a.id, a.task_id, t.title, u.display_name, a.kind, a.detail, \
             a.created_at, t.task_key FROM activity a \
             LEFT JOIN task t ON t.id = a.task_id \
             LEFT JOIN user u ON u.id = a.actor_id";
        let conn = self.conn.lock().await;
        let (filter_sql, filter_vals) = activity_filter_sql(filter)?;
        // The base order is the feed's own reading direction; `After` scans
        // the opposite way (back toward the start) and the caller reverses
        // the page below to restore it.
        let base = match dir {
            Dir::Newest => "DESC",
            Dir::Oldest => "ASC",
        };
        let opp = match dir {
            Dir::Newest => "ASC",
            Dir::Oldest => "DESC",
        };
        let reverse = matches!(page, FeedPage::After(_));
        let mut vals: Vec<Value> = Vec::new();
        let sql = match &page {
            FeedPage::Newest => {
                vals.extend(filter_vals);
                vals.push(i64::from(limit).into());
                format!(
                    "{SELECT} WHERE 1=1{filter_sql} ORDER BY a.created_at {base}, a.id {base} LIMIT ?"
                )
            }
            FeedPage::Before(cursor) => {
                // Further along in `dir`'s reading order: older when
                // Newest, newer when Oldest.
                let cmp = match dir {
                    Dir::Newest => "<",
                    Dir::Oldest => ">",
                };
                let at = stamp(cursor.at)?;
                vals.push(at.clone().into());
                vals.push(at.into());
                vals.push(cursor.id.clone().into());
                vals.extend(filter_vals);
                vals.push(i64::from(limit).into());
                format!(
                    "{SELECT} WHERE (a.created_at {cmp} ? OR (a.created_at = ? AND a.id {cmp} ?)){filter_sql} \
                     ORDER BY a.created_at {base}, a.id {base} LIMIT ?"
                )
            }
            FeedPage::After(cursor) => {
                // Back toward the start: newer when Newest, older when
                // Oldest — scanned in the opposite order, then reversed.
                let cmp = match dir {
                    Dir::Newest => ">",
                    Dir::Oldest => "<",
                };
                let at = stamp(cursor.at)?;
                vals.push(at.clone().into());
                vals.push(at.into());
                vals.push(cursor.id.clone().into());
                vals.extend(filter_vals);
                vals.push(i64::from(limit).into());
                format!(
                    "{SELECT} WHERE (a.created_at {cmp} ? OR (a.created_at = ? AND a.id {cmp} ?)){filter_sql} \
                     ORDER BY a.created_at {opp}, a.id {opp} LIMIT ?"
                )
            }
        };
        let mut rows = conn.query(&sql, vals).await.map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(ActivityLine {
                id: text(&row, 0)?,
                task_id: opt_text(&row, 1)?,
                title: opt_text(&row, 2)?,
                actor_name: opt_text(&row, 3)?,
                kind: ActivityKind::parse(&text(&row, 4)?),
                detail: text(&row, 5)?,
                at: parse_stamp(&text(&row, 6)?)?,
                task_key: opt_text(&row, 7)?,
            });
        }
        if reverse {
            out.reverse();
        }
        Ok(out)
    }

    async fn count_activity(&self, filter: &ActivityFilter) -> Result<u64> {
        let (filter_sql, filter_vals) = activity_filter_sql(filter)?;
        let sql = format!(
            "SELECT COUNT(*) FROM activity a \
             LEFT JOIN task t ON t.id = a.task_id \
             LEFT JOIN user u ON u.id = a.actor_id WHERE 1=1{filter_sql}"
        );
        let row = self.one_row(&sql, filter_vals).await?;
        row.as_ref().map(count_of).unwrap_or(Ok(0))
    }

    async fn count_activity_preceding(
        &self,
        filter: &ActivityFilter,
        dir: Dir,
        cursor: Option<&FeedCursor>,
    ) -> Result<u64> {
        let Some(cursor) = cursor else { return Ok(0) };
        let (filter_sql, filter_vals) = activity_filter_sql(filter)?;
        // Preceding = already shown on an earlier page: back toward the
        // start of `dir`'s reading order, the same side `FeedPage::After`
        // reads from.
        let cmp = match dir {
            Dir::Newest => ">",
            Dir::Oldest => "<",
        };
        let at = stamp(cursor.at)?;
        let mut vals: Vec<Value> = vec![at.clone().into(), at.into(), cursor.id.clone().into()];
        vals.extend(filter_vals);
        let sql = format!(
            "SELECT COUNT(*) FROM activity a \
             LEFT JOIN task t ON t.id = a.task_id \
             LEFT JOIN user u ON u.id = a.actor_id \
             WHERE (a.created_at {cmp} ? OR (a.created_at = ? AND a.id {cmp} ?)){filter_sql}"
        );
        let row = self.one_row(&sql, vals).await?;
        row.as_ref().map(count_of).unwrap_or(Ok(0))
    }

    async fn task_directory(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT task_key, title FROM task WHERE deleted_at IS NULL ORDER BY task_key",
                (),
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push((text(&row, 0)?, text(&row, 1)?));
        }
        Ok(out)
    }

    // -- who gets mailed ---------------------------------------------------

    async fn recipients_for_task(&self, task_id: &str) -> Result<Vec<Recipient>> {
        let conn = self.conn.lock().await;
        // The role filter is belt as well as braces: a Viewer cannot be
        // assigned in the first place, and if one ever were, no mail would go
        // out to them from here.
        let mut rows = conn
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
        let conn = self.conn.lock().await;
        let mut rows = conn
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

    async fn hold_batch(
        &self,
        task_id: &str,
        recipient: &str,
        until: OffsetDateTime,
        cap: Duration,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        // Read the rows and their birth moments first: the ceiling is per
        // row — the oldest mail in the batch is the one whose patience runs
        // out — and expressing that in SQL over RFC 3339 text would be a
        // date-format trick rather than a calculation.
        let mut rows = conn
            .query(
                "SELECT id, claimed_at, next_attempt_at FROM mail_send \
                 WHERE task_id = ?1 AND recipient = ?2 AND kind = 'rule' \
                   AND state = 'pending' AND attempts = 0 \
                   AND next_attempt_at IS NOT NULL",
                params![task_id, recipient],
            )
            .await
            .map_err(backend)?;
        let mut held = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            let id = text(&row, 0)?;
            let born = parse_stamp(&text(&row, 1)?)?;
            let due = parse_stamp(&text(&row, 2)?)?;
            held.push((id, born, due));
        }
        drop(rows);
        for (id, born, due) in held {
            let ceiling = born + cap;
            let when = if until > ceiling { ceiling } else { until };
            // Only ever later. A row a delivery pass is holding — its lease is
            // written in this same column — must not be pulled back under the
            // pass that took it, or the mail it is composing right now becomes
            // due for a second pass and goes out twice. The `<` in the update
            // is the guard, not the read above it: the two are one statement.
            if when <= due {
                continue;
            }
            conn.execute(
                "UPDATE mail_send SET next_attempt_at = ?1 \
                 WHERE id = ?2 AND state = 'pending' AND attempts = 0 \
                   AND next_attempt_at < ?1",
                params![stamp(when)?, id],
            )
            .await
            .map_err(backend)?;
        }
        drop(conn);
        self.announce([Topic::Queue]);
        Ok(())
    }

    async fn recipients_for_task_creator(&self, task_id: &str) -> Result<Vec<Recipient>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
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

    async fn recipient(&self, user_id: &str) -> Result<Option<Recipient>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT u.id, u.email, u.display_name FROM user u \
                 WHERE u.id = ?1 AND u.role <> ?2",
                params![user_id, Role::Viewer.as_str()],
            )
            .await
            .map_err(backend)?;
        Ok(recipients_from(&mut rows).await?.into_iter().next())
    }

    // -- tags --------------------------------------------------------------

    async fn tags(&self, board_id: &str) -> Result<Vec<Tag>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT id, board_id, name, position, is_default FROM tag WHERE board_id = ?1 \
                 ORDER BY position",
                params![board_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push(Tag {
                id: text(&row, 0)?,
                board_id: text(&row, 1)?,
                name: text(&row, 2)?,
                position: row.get::<i64>(3).map_err(backend)?,
                is_default: row.get::<i64>(4).map_err(backend)? != 0,
            });
        }
        Ok(out)
    }

    async fn tag_task_counts(&self, board_id: &str) -> Result<Vec<(String, u32)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT tag_id, COUNT(*) FROM task \
                 WHERE board_id = ?1 AND deleted_at IS NULL GROUP BY tag_id",
                params![board_id],
            )
            .await
            .map_err(backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            out.push((text(&row, 0)?, row.get::<i64>(1).map_err(backend)? as u32));
        }
        Ok(out)
    }

    async fn create_tag(&self, board_id: &str, name: &str, at: OffsetDateTime) -> Result<Tag> {
        let id = Ulid::new().to_string();
        let now = stamp(at)?;
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;

        // A new tag lands at the end of the board's order. The name is the
        // unique index's to police — a pre-read would only widen the race.
        let written = async {
            let mut rows = tx
                .query(
                    "SELECT COALESCE(MAX(position), 0) FROM tag WHERE board_id = ?1",
                    params![board_id],
                )
                .await?;
            let position = match rows.next().await? {
                Some(row) => row.get::<i64>(0).unwrap_or(0) + 1,
                None => 1,
            };
            drop(rows);
            tx.execute(
                "INSERT INTO tag (id, board_id, name, position, is_default, created_at) \
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                params![id.clone(), board_id, name, position, now],
            )
            .await?;
            Ok::<_, turso::Error>(position)
        }
        .await;

        let position = match written {
            Ok(position) => position,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(if is_constraint_violation(&e) {
                    StoreError::Conflict("tag")
                } else {
                    backend(e)
                });
            }
        };
        tx.commit().await.map_err(backend)?;

        self.announce([Topic::Board, Topic::Tags]);
        Ok(Tag {
            id,
            board_id: board_id.to_string(),
            name: name.to_string(),
            position,
            is_default: false,
        })
    }

    async fn rename_tag(&self, tag_id: &str, name: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE tag SET name = ?1 WHERE id = ?2",
                params![name, tag_id],
            )
            .await
            .map_err(|e| {
                if is_constraint_violation(&e) {
                    StoreError::Conflict("tag")
                } else {
                    backend(e)
                }
            })?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        drop(conn);
        self.announce([Topic::Board, Topic::Tags]);
        Ok(())
    }

    async fn delete_tag(&self, tag_id: &str) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        // The default is the board's fallback, not a project anyone can
        // retire: deleting it would leave its board's tasks nowhere to go.
        // Any other tag is deletable only while it is empty — a tag with
        // cards on it is a project somebody is working in, and a delete that
        // quietly re-files that work is not a delete they asked for. Cards
        // already thrown away are the exception: they are nobody's work any
        // more, and they move to the default so the reference they still hold
        // points at something.
        let deleted = async {
            let mut rows = tx
                .query(
                    "SELECT board_id, is_default FROM tag WHERE id = ?1",
                    params![tag_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok::<_, turso::Error>(Err(StoreError::NotFound));
            };
            let board_id: String = row.get(0)?;
            let is_default: i64 = row.get(1)?;
            drop(rows);
            if is_default != 0 {
                return Ok(Err(StoreError::Conflict("default_tag")));
            }
            let mut rows = tx
                .query(
                    "SELECT COUNT(*) FROM task WHERE tag_id = ?1 AND deleted_at IS NULL",
                    params![tag_id],
                )
                .await?;
            let live: i64 = match rows.next().await? {
                Some(row) => row.get(0)?,
                None => 0,
            };
            drop(rows);
            if live > 0 {
                return Ok(Err(StoreError::Conflict("tag_in_use")));
            }
            tx.execute(
                "UPDATE task SET tag_id = \
                 (SELECT id FROM tag WHERE board_id = ?1 AND is_default = 1) \
                 WHERE tag_id = ?2",
                params![board_id, tag_id],
            )
            .await?;
            tx.execute("DELETE FROM tag WHERE id = ?1", params![tag_id])
                .await?;
            Ok(Ok(()))
        }
        .await;
        match deleted {
            Ok(Ok(())) => {}
            Ok(Err(refused)) => {
                let _ = tx.rollback().await;
                return Err(refused);
            }
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(backend(e));
            }
        }
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Board, Topic::Tags]);
        Ok(())
    }

    async fn move_tag(&self, tag_id: &str, up: bool) -> Result<()> {
        let mut conn = self.tx_conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let written = async {
            let mut rows = tx
                .query(
                    "SELECT board_id, position FROM tag WHERE id = ?1",
                    params![tag_id],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok::<_, turso::Error>(Err(StoreError::NotFound));
            };
            let board_id: String = row.get(0)?;
            let position: i64 = row.get(1)?;
            drop(rows);
            let other = if up { position - 1 } else { position + 1 };
            let mut rows = tx
                .query(
                    "SELECT id FROM tag WHERE board_id = ?1 AND position = ?2",
                    params![board_id, other],
                )
                .await?;
            let Some(neighbour) = rows.next().await? else {
                // Already at that end of the order: nothing to swap, and that
                // is not an error.
                return Ok(Ok(()));
            };
            let neighbour_id: String = neighbour.get(0)?;
            drop(rows);
            tx.execute(
                "UPDATE tag SET position = ?1 WHERE id = ?2",
                params![position, neighbour_id],
            )
            .await?;
            tx.execute(
                "UPDATE tag SET position = ?1 WHERE id = ?2",
                params![other, tag_id],
            )
            .await?;
            Ok(Ok(()))
        }
        .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(refused)) => {
                let _ = tx.rollback().await;
                return Err(refused);
            }
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(backend(e));
            }
        }
        tx.commit().await.map_err(backend)?;
        self.announce([Topic::Board, Topic::Tags]);
        Ok(())
    }

    async fn set_task_tag(&self, task_id: &str, tag_id: &str) -> Result<()> {
        // A tag not of this task's board is NotFound, not refused: the
        // asker named a thing that is not one of this board's projects.
        let known = self
            .one_row(
                "SELECT g.id FROM tag g JOIN task t ON t.board_id = g.board_id \
                 WHERE g.id = ?1 AND t.id = ?2 AND t.deleted_at IS NULL",
                params![tag_id, task_id],
            )
            .await?;
        if known.is_none() {
            return Err(StoreError::NotFound);
        }
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE task SET tag_id = ?1 WHERE id = ?2 AND deleted_at IS NULL",
                params![tag_id, task_id],
            )
            .await
            .map_err(backend)?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        drop(conn);
        self.announce([Topic::Board, Topic::Task(task_id.to_string()), Topic::Tags]);
        Ok(())
    }
}

#[async_trait]
impl DetailReads for TursoStore {
    async fn task(&self, task_id: &str) -> Result<Option<TaskFacts>> {
        let row = self
            .one_row(
                "SELECT t.id, t.task_key, t.title, t.column_id, t.deadline, t.clock_at, \
                 t.position, t.done_at, t.description, t.board_id, b.workspace_id, \
                 t.parent_id, t.tag_id, g.name \
                 FROM task t LEFT JOIN tag g ON g.id = t.tag_id \
                 JOIN board b ON b.id = t.board_id \
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
                clock_at: opt_stamp(&row, 5)?,
                position: row.get::<f64>(6).map_err(backend)?,
                done_at: opt_stamp(&row, 7)?,
                parent_id: opt_text(&row, 11)?,
                tag: match opt_text(&row, 12)? {
                    Some(id) => Some(TagChip {
                        id,
                        name: text(&row, 13)?,
                    }),
                    None => None,
                },
            },
            description: text(&row, 8)?,
            board_id: text(&row, 9)?,
            workspace_id: text(&row, 10)?,
        }))
    }

    async fn columns_for_board(&self, board_id: &str) -> Result<Vec<Column>> {
        BoardReads::columns(self, board_id).await
    }

    async fn assignees_for_task(&self, task_id: &str) -> Result<Vec<Person>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT u.id, u.display_name, (u.photo_mime IS NOT NULL) FROM task_assignee a \
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
                has_photo: row.get::<i64>(2).map_err(backend)? != 0,
            });
        }
        Ok(out)
    }

    async fn assignable_people(&self, workspace_id: &str) -> Result<Vec<Person>> {
        let conn = self.conn.lock().await;
        // Id, name and photo only: the picker shows no addresses and no roles,
        // so neither leaves the server for this screen.
        let mut rows = conn
            .query(
                "SELECT id, display_name, (photo_mime IS NOT NULL) FROM user \
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
                has_photo: row.get::<i64>(2).map_err(backend)? != 0,
            });
        }
        Ok(out)
    }

    async fn dependencies_for_task(&self, task_id: &str) -> Result<Vec<(bool, DependencyEdge)>> {
        let conn = self.conn.lock().await;
        // Both directions in one round trip: the leading column says which.
        // `cleared_at` is only ever set by an unlink, so a cleared row is not a
        // link any more and does not belong on the screen. A link whose blocker
        // is finished still shows — that is `done_at`, and it reads as cleared.
        let mut rows = conn
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
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT c.id, c.body, c.created_at, u.id, u.display_name, (u.photo_mime IS NOT NULL) \
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
                    has_photo: row.get::<i64>(5).map_err(backend)? != 0,
                },
            });
        }
        Ok(out)
    }

    async fn files_for_task(&self, task_id: &str) -> Result<Vec<FileLine>> {
        let conn = self.conn.lock().await;
        // `bytes` is not in the SELECT on purpose: a screen listing five files
        // must not drag five files through memory to print their names.
        let mut rows = conn
            .query(
                "SELECT id, file_name, size_bytes, comment_id, uploaded_by, mime_type \
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
                mime_type: text(&row, 5)?,
            });
        }
        Ok(out)
    }

    async fn activity_for_task(&self, task_id: &str) -> Result<Vec<ActivityEntry>> {
        let conn = self.conn.lock().await;
        // LEFT JOIN: a line the system wrote has no actor, and dropping it
        // would hide exactly the events the rules engine causes.
        let mut rows = conn
            .query(
                "SELECT a.id, a.kind, a.detail, a.created_at, u.id, u.display_name, \
                 (u.photo_mime IS NOT NULL) \
                 FROM activity a LEFT JOIN user u ON u.id = a.actor_id \
                 WHERE a.task_id = ?1 ORDER BY a.created_at DESC, a.rowid DESC",
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
                    has_photo: row.get::<i64>(6).map_err(backend)? != 0,
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

    async fn family_for_task(&self, task_id: &str) -> Result<Vec<(bool, SubtaskLine)>> {
        let conn = self.conn.lock().await;
        // One query for both directions, and one for the assignees too: the
        // LEFT JOIN repeats a task's row once per person it points at, and the
        // fold below puts them back together. A task detail is not allowed to
        // cost a query per subtask.
        let mut rows = conn
            .query(
                "SELECT t.id, t.task_key, t.title, t.column_id, t.done_at, t.parent_id, \
                 u.id, u.display_name, (u.photo_mime IS NOT NULL) \
                 FROM task t \
                 LEFT JOIN task_assignee a ON a.task_id = t.id \
                 LEFT JOIN user u ON u.id = a.user_id \
                 WHERE t.deleted_at IS NULL AND ( \
                   t.parent_id = ?1 \
                   OR t.id = (SELECT parent_id FROM task WHERE id = ?1) \
                 ) \
                 ORDER BY t.created_at, u.display_name",
                params![task_id],
            )
            .await
            .map_err(backend)?;

        let mut out: Vec<(bool, SubtaskLine)> = Vec::new();
        while let Some(row) = rows.next().await.map_err(backend)? {
            let id = text(&row, 0)?;
            // A row whose parent is the task we asked about is one of its
            // parts; anything else this query can return is the parent.
            let is_parent = opt_text(&row, 5)?.as_deref() != Some(task_id);
            let person = match opt_text(&row, 6)? {
                Some(user_id) => Some(Person {
                    id: user_id,
                    display_name: text(&row, 7)?,
                    has_photo: row.get::<i64>(8).map_err(backend)? != 0,
                }),
                None => None,
            };
            match out.last_mut() {
                Some((_, line)) if line.id == id => {
                    if let Some(person) = person {
                        line.assignees.push(person);
                    }
                }
                _ => out.push((
                    is_parent,
                    SubtaskLine {
                        id,
                        task_key: text(&row, 1)?,
                        title: text(&row, 2)?,
                        column_id: text(&row, 3)?,
                        done_at: opt_stamp(&row, 4)?,
                        assignees: person.into_iter().collect(),
                    },
                )),
            }
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
        let conn = self.conn.lock().await;
        let mut rows = conn
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
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT t.id, t.task_key, t.title, t.column_id, t.deadline, t.clock_at, \
                 t.position, t.done_at, t.parent_id, t.tag_id, g.name \
                 FROM task t LEFT JOIN tag g ON g.id = t.tag_id \
                 WHERE t.board_id = ?1 AND t.deleted_at IS NULL",
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
                clock_at: opt_stamp(&row, 5)?,
                position: row.get::<f64>(6).map_err(backend)?,
                done_at: opt_stamp(&row, 7)?,
                parent_id: opt_text(&row, 8)?,
                tag: match opt_text(&row, 9)? {
                    Some(id) => Some(TagChip {
                        id,
                        name: text(&row, 10)?,
                    }),
                    None => None,
                },
            });
        }
        Ok(out)
    }

    async fn assignees_for_board(&self, board_id: &str) -> Result<Vec<(String, Person)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT a.task_id, u.id, u.display_name, (u.photo_mime IS NOT NULL) \
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
                    has_photo: row.get::<i64>(3).map_err(backend)? != 0,
                },
            ));
        }
        Ok(out)
    }

    async fn comment_counts_for_board(&self, board_id: &str) -> Result<Vec<(String, u32)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
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
        let conn = self.conn.lock().await;
        let mut rows = conn
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
        let dir = std::env::temp_dir().join(format!("izlek-probe-{}", Ulid::new()));
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
        let dir = std::env::temp_dir().join(format!("izlek-probe-{}", Ulid::new()));
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
        let dir = std::env::temp_dir().join(format!("izlek-probe-{}", Ulid::new()));
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
        let dir = std::env::temp_dir().join(format!("izlek-sender-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TursoStore::open(dir.join("izlek.db").to_str().unwrap(), &dir.join("storage"))
            .await
            .unwrap();
        use crate::store::Store as _;

        let (ws, _admin) = store
            .claim_workspace("İzlek", "ada@izlek.sh", "Ada", "hash")
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
                    from_name: "İzlek".into(),
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

/// The storage tree is the other half of the state the rows describe. These
/// drive the file half through the same [`Store`] API the handlers use: a
/// row's bytes live at exactly one path named by the row's id, a delete
/// takes the file with it, and a boot sweep collects what the rows stopped
/// naming. Nothing binary is in the database any more — the premise a
/// missing file is recoverable-by-reread no longer holds, and these are
/// where that shows up if it breaks.
#[cfg(test)]
mod storage_fs {
    use super::*;

    /// A file-backed store and its storage tree, both under one tempdir the
    /// fixture keeps alive. File-backed because the boot passes (reconcile,
    /// resniff, sweep) only run for a database that survives its handle.
    struct Fixture {
        store: TursoStore,
        dir: tempfile::TempDir,
    }

    impl Fixture {
        async fn open() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = TursoStore::open(
                dir.path().join("izlek.db").to_str().unwrap(),
                &dir.path().join("storage"),
            )
            .await
            .unwrap();
            Self { store, dir }
        }

        fn storage(&self) -> std::path::PathBuf {
            self.dir.path().join("storage")
        }

        /// A workspace with its admin, and one task on the default board.
        async fn with_task(&self) -> (String, String, String) {
            let (ws, admin) = self
                .store
                .claim_workspace("İzlek", "ada@izlek.sh", "Ada", "hash")
                .await
                .unwrap();
            let admin = admin.id;
            let board = self.store.board(&ws.id).await.unwrap().unwrap();
            let column = self
                .store
                .columns(&board.id)
                .await
                .unwrap()
                .into_iter()
                .find(|c| !c.is_done)
                .unwrap_or_else(|| panic!("no open column"))
                .id;
            let task = self
                .store
                .create_task(NewTask {
                    clock_at: None,
                    board_id: &board.id,
                    column_id: &column,
                    parent_id: None,
                    title: "a task",
                    description: "",
                    deadline: None,
                    created_by: &admin,
                })
                .await
                .unwrap()
                .row
                .id;
            (ws.id, admin, task)
        }
    }

    #[tokio::test]
    async fn an_attachment_round_trips_through_its_file() {
        let fx = Fixture::open().await;
        let (_ws, admin, task) = fx.with_task().await;
        let bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0xff, 0x00];
        let id = fx
            .store
            .add_attachment(NewAttachment {
                task_id: &task,
                comment_id: None,
                file_name: "shot.png",
                mime_type: "image/png",
                bytes: bytes.clone(),
                uploaded_by: &admin,
                at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();

        assert_eq!(
            fx.store.attachment_bytes(&id).await.unwrap().unwrap(),
            bytes
        );
        // The bytes are a file the row's id names, under attachments/, and
        // nothing binary went into the database: the temp file the write
        // passed through is gone, and the tree holds exactly one file.
        let file = attachment_file(&fx.storage(), &id);
        assert!(file.is_file(), "the attachment file exists");
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        let dir = fx.storage().join(ATTACHMENTS_DIR);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_photo_round_trips_through_its_file() {
        let fx = Fixture::open().await;
        let (_ws, admin, _task) = fx.with_task().await;
        fx.store
            .set_photo(&admin, b"grace-bytes", "image/png")
            .await
            .unwrap();

        let photo = fx.store.photo(&admin).await.unwrap().unwrap();
        assert_eq!(photo.0, b"grace-bytes".to_vec());
        assert_eq!(photo.1, "image/png");
        let file = photo_file(&fx.storage(), &admin);
        assert!(file.is_file(), "the photo file exists");
        assert_eq!(std::fs::read(&file).unwrap(), b"grace-bytes");
    }

    #[tokio::test]
    async fn deleting_an_attachment_takes_its_file_with_it() {
        let fx = Fixture::open().await;
        let (_ws, admin, task) = fx.with_task().await;
        let id = fx
            .store
            .add_attachment(NewAttachment {
                task_id: &task,
                comment_id: None,
                file_name: "note.txt",
                mime_type: "text/plain",
                bytes: b"hello".to_vec(),
                uploaded_by: &admin,
                at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        let file = attachment_file(&fx.storage(), &id);

        assert!(fx.store.delete_attachment(&id).await.unwrap());
        assert!(!file.exists(), "the file went with the row");
        assert!(fx.store.attachment(&id).await.unwrap().is_none());
        assert!(fx.store.attachment_bytes(&id).await.unwrap().is_none());
        // A second delete is still a no-op, file and all.
        assert!(!fx.store.delete_attachment(&id).await.unwrap());
    }

    #[tokio::test]
    async fn clearing_a_photo_takes_its_file_with_it() {
        let fx = Fixture::open().await;
        let (_ws, admin, _task) = fx.with_task().await;
        fx.store
            .set_photo(&admin, b"grace-bytes", "image/png")
            .await
            .unwrap();
        let file = photo_file(&fx.storage(), &admin);

        fx.store.clear_photo(&admin).await.unwrap();
        assert!(!file.exists(), "the file went with the marker");
        assert!(fx.store.photo(&admin).await.unwrap().is_none());
        assert!(!fx.store.user(&admin).await.unwrap().unwrap().has_photo);
    }

    #[tokio::test]
    async fn the_boot_sweep_deletes_files_no_row_names() {
        let fx = Fixture::open().await;
        let attachments = fx.storage().join(ATTACHMENTS_DIR);
        let photos = fx.storage().join(PHOTOS_DIR);
        // Strays in both subdirs: a file under a made-up id, and the temp
        // file a crash abandoned mid-write.
        std::fs::write(attachments.join("01AAAAAAAAAAAAAAAAAAAAAAAA"), b"x").unwrap();
        std::fs::write(attachments.join("02BBBBBBBBBBBBBBBBBBBBBBBB.tmp"), b"x").unwrap();
        std::fs::write(photos.join("03CCCCCCCCCCCCCCCCCCCCCCCC"), b"x").unwrap();

        // A reopen is a boot; the sweep runs inside it.
        let Fixture { store, dir } = fx;
        drop(store);
        let db_path = dir.path().join("izlek.db").to_str().unwrap().to_string();
        let storage = dir.path().join("storage");
        let store = TursoStore::open(&db_path, &storage).await.unwrap();
        assert_eq!(std::fs::read_dir(&attachments).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&photos).unwrap().count(), 0);
        drop(store);
    }

    #[tokio::test]
    async fn a_row_whose_file_is_missing_is_kept_and_said_out_loud() {
        let fx = Fixture::open().await;
        let (_ws, admin, task) = fx.with_task().await;
        let id = fx
            .store
            .add_attachment(NewAttachment {
                task_id: &task,
                comment_id: None,
                file_name: "shot.png",
                mime_type: "image/png",
                bytes: b"payload".to_vec(),
                uploaded_by: &admin,
                at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        fx.store
            .set_photo(&admin, b"grace-bytes", "image/png")
            .await
            .unwrap();
        // The files are lost the way a botched restore loses them: rows on
        // disk, files gone.
        std::fs::remove_file(attachment_file(&fx.storage(), &id)).unwrap();
        std::fs::remove_file(photo_file(&fx.storage(), &admin)).unwrap();

        let Fixture { store, dir } = fx;
        drop(store);
        let db_path = dir.path().join("izlek.db").to_str().unwrap().to_string();
        let storage = dir.path().join("storage");
        let store = TursoStore::open(&db_path, &storage).await.unwrap();
        // Both rows survive the sweep; the reads answer "nothing to serve".
        assert!(store.attachment(&id).await.unwrap().is_some());
        assert!(store.attachment_bytes(&id).await.unwrap().is_none());
        let admin_row = store.user(&admin).await.unwrap().unwrap();
        assert!(admin_row.has_photo, "the row still says a photo is there");
        assert!(store.photo(&admin).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_boot_resniff_reads_its_windows_from_the_file() {
        let fx = Fixture::open().await;
        let (_ws, admin, task) = fx.with_task().await;
        // A zip whose entry names name a presentation. Uploaded with the
        // generic bucket a pre-sniffer write froze on — the store does not
        // sniff on the way in, so the row wears it until a boot pass runs.
        let bytes = zip_with(&["ppt/presentation.xml", "ppt/slides/slide1.xml"]);
        let id = fx
            .store
            .add_attachment(NewAttachment {
                task_id: &task,
                comment_id: None,
                file_name: "old.pptx",
                mime_type: "application/zip",
                bytes,
                uploaded_by: &admin,
                at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        let Fixture { store, dir } = fx;
        drop(store);
        let db_path = dir.path().join("izlek.db").to_str().unwrap().to_string();
        let storage = dir.path().join("storage");
        let store = TursoStore::open(&db_path, &storage).await.unwrap();
        assert_eq!(
            store.attachment(&id).await.unwrap().unwrap().mime_type,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "the windows came off the file, and they settled the type"
        );
    }

    /// A zip of the named entries, contents irrelevant: the sniffer reads
    /// the entry names a zip stores uncompressed.
    fn zip_with(names: &[&str]) -> Vec<u8> {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for name in names {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"<xml/>").unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
}
