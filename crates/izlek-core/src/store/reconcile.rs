//! Reconcile a live Turso database with the declared whole schema.
//!
//! The live file is never altered in place. Instead a new file is built from
//! `migrations/0001_init.sql`, the data is copied with an explicit per-table
//! column map, the copy is verified, and only then the files are swapped. The
//! original file is kept as a timestamped backup and is never deleted.

use super::schema::{SCHEMA, declared_fingerprint, diff_report, fingerprint};
use super::{Result, StoreError};
use turso::{Builder, Connection, params};
use ulid::Ulid;

/// How the caller wants to confirm the rebuild.
pub struct ReconcileOptions {
    /// Print the diff and plan, then stop without touching anything.
    pub dry_run: bool,
    /// Skip the interactive confirmation.
    pub yes: bool,
    /// Boot path: do not prompt, log what happened and proceed.
    pub auto: bool,
}

/// Rebuilds the database at `path` to match the declared schema.
///
/// - Empty or already-current databases are a no-op.
/// - A differing database is backed up, rebuilt, verified and swapped into
///   place.
/// - A rebuild that fails verification deletes the `.rebuilt` file and leaves
///   the original untouched.
pub async fn reconcile(path: &str, opts: ReconcileOptions) -> Result<()> {
    if path == ":memory:" {
        return Err(StoreError::Backend(
            "reconcile is not meaningful on an in-memory database".into(),
        ));
    }
    let db_path = std::path::Path::new(path);
    if !db_path.exists() {
        return Err(StoreError::Backend(format!("database not found: {}", path)));
    }

    // Open the live database read-only to inspect its fingerprint.
    let old_db = Builder::new_local(path)
        .build()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let old_conn = old_db
        .connect()
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    let old_fp = fingerprint(&old_conn).await?;
    let new_fp = declared_fingerprint().await?;

    if old_fp == new_fp {
        if !opts.auto {
            println!("database already matches the declared schema");
        }
        return Ok(());
    }

    let diff = diff_report(&old_fp, &new_fp);

    if opts.dry_run {
        println!("schema difference:\n{}", diff);
        println!("dry run: would rebuild {}", path);
        return Ok(());
    }

    if opts.auto {
        eprintln!("database schema differs from declared schema; rebuilding automatically");
        eprintln!("difference:\n{}", diff);
    } else {
        println!("schema difference:\n{}", diff);
        if !opts.yes {
            print!("rebuild {}? [y/N] ", path);
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut answer = String::new();
            match std::io::stdin().read_line(&mut answer) {
                Ok(_) if answer.trim().eq_ignore_ascii_case("y") => {}
                _ => {
                    println!("rebuild cancelled");
                    return Ok(());
                }
            }
        }
    }

    let rebuilt_path = format!("{}.rebuilt", path);
    cleanup_rebuilt(&rebuilt_path);

    let result = rebuild(path, &rebuilt_path, &old_conn).await;

    // Close the read-only connection before any file moves; this also
    // releases the WAL locks on the original file.
    drop(old_conn);
    drop(old_db);

    if let Err(e) = result {
        cleanup_rebuilt(&rebuilt_path);
        return Err(e);
    }

    let backup_path = backup_name(path)?;
    std::fs::rename(path, &backup_path).map_err(|e| {
        StoreError::Backend(format!(
            "failed to move original database to backup {}: {}",
            backup_path, e
        ))
    })?;
    // SQLite WAL/SHM siblings belong to the main file; the backup must be a
    // complete database on its own, and the rebuilt file starts with none.
    rename_sibling(path, &backup_path, "-wal");
    rename_sibling(path, &backup_path, "-shm");
    if let Err(e) = std::fs::rename(&rebuilt_path, path) {
        // Best-effort undo so the live file is not gone. If this also fails,
        // the caller has both the backup and the rebuilt file to recover from.
        let _ = std::fs::rename(&backup_path, path);
        let _ = rename_sibling(&backup_path, path, "-wal");
        let _ = rename_sibling(&backup_path, path, "-shm");
        return Err(StoreError::Backend(format!(
            "failed to move rebuilt database into place {}: {}",
            rebuilt_path, e
        )));
    }
    // The rebuilt file was checkpointed before the swap, so its own sidecars
    // hold nothing — but a `.rebuilt-wal` left lying about would be adopted by
    // the NEXT rebuild's fresh `.rebuilt` file, which is how a stale WAL
    // corrupts a database that was otherwise fine.
    let _ = std::fs::remove_file(format!("{}-wal", rebuilt_path));
    let _ = std::fs::remove_file(format!("{}-shm", rebuilt_path));

    if opts.auto {
        eprintln!(
            "database rebuilt and verified; original backed up to {}",
            backup_path
        );
        eprintln!("rebuilt database now at {}", path);
    } else {
        println!("database rebuilt and verified");
        println!("original backed up to {}", backup_path);
        println!("rebuilt database now at {}", path);
    }

    Ok(())
}

/// Does the actual rebuild: creates `.rebuilt`, copies the data (topping up
/// a default tag for any board that comes across with none), and verifies
/// the result. On success the rebuilt file is complete and checkpointed; on
/// failure the caller deletes it.
async fn rebuild(path: &str, rebuilt_path: &str, old_conn: &Connection) -> Result<()> {
    // The copy runs as `INSERT ... SELECT FROM old.<table>`, which needs
    // ATTACH — still gated in this engine, and switched on only here, for the
    // one connection that does the rebuild. The verification afterwards is
    // what protects the data, not the copy's mechanism.
    let new_db = Builder::new_local(rebuilt_path)
        .experimental_attach(true)
        .build()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let new_conn = new_db
        .connect()
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    new_conn
        .execute("PRAGMA foreign_keys = ON", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    new_conn
        .execute_batch(SCHEMA)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    copy_data(old_conn, &new_conn, path).await?;
    verify(old_conn, &new_conn).await?;

    // A checkpoint PRAGMA answers with a row, so it is a query: `execute`
    // treats a row as a failure ("unexpected row during execution").
    let mut checkpoint = new_conn
        .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
        .await
        .map_err(|e| StoreError::Backend(format!("checkpoint: {e}")))?;
    while checkpoint
        .next()
        .await
        .map_err(|e| StoreError::Backend(format!("checkpoint: {e}")))?
        .is_some()
    {}

    Ok(())
}

/// Seeds one `General` default tag into every board that, after the old tags
/// were copied, still has none. A database that carried tags loses nothing
/// here — only a board the copy left without a default is topped up, so
/// `tag_one_default` can never be violated. It runs after `board` and `tag`
/// and before `task`: boards and tags must be in for the foreign keys, and a
/// pre-tag database's task map fills `tag_id` from exactly these defaults.
async fn seed_general_tags(new_conn: &Connection) -> Result<()> {
    // One connection sees both databases here: `main` is the rebuild, `old`
    // the attached file being copied from.
    let mut rows = new_conn
        .query(
            "SELECT b.id, b.created_at FROM old.board AS b \
             WHERE NOT EXISTS (SELECT 1 FROM main.tag WHERE board_id = b.id AND is_default = 1)",
            (),
        )
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let mut boards = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let board_id: String = row.get(0).map_err(|e| StoreError::Backend(e.to_string()))?;
        let created_at: String = row.get(1).map_err(|e| StoreError::Backend(e.to_string()))?;
        boards.push((board_id, created_at));
    }
    for (board_id, created_at) in boards {
        let id = Ulid::new().to_string();
        new_conn
            .execute(
                "INSERT INTO main.tag (id, board_id, name, position, is_default, created_at) \
                 VALUES (?1, ?2, 'General', 0, 1, ?3)",
                params![id, board_id, created_at],
            )
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
    }
    Ok(())
}

/// A per-table column map. Every destination column must appear exactly once;
/// `copy_data` validates this against the declared schema before inserting.
struct TableMap {
    name: &'static str,
    columns: Vec<(&'static str, String)>,
}

/// Builds the explicit column map. Unchanged tables map every column to
/// `old.<col>`; the changed tables carry the migration-specific expressions.
fn build_maps(
    old_has_smtp_check: bool,
    old_has_batch_window: bool,
    old_has_clock: bool,
    old_has_reminder: bool,
    old_has_feed_seen: bool,
    old_has_tag: bool,
) -> Vec<TableMap> {
    let mut maps = Vec::new();

    maps.push(TableMap {
        name: "workspace",
        columns: vec![
            ("id", "old.id".into()),
            ("name", "old.name".into()),
            ("created_at", "old.created_at".into()),
            (
                "attachment_limit_bytes",
                "old.attachment_limit_bytes".into(),
            ),
            ("photo_limit_bytes", "old.photo_limit_bytes".into()),
            ("allowed_file_types", "old.allowed_file_types".into()),
            (
                // A database written before notifications were batched has no
                // window of its own; it starts on the schema's own default
                // rather than on zero, which would keep the old behaviour by
                // accident and make the feature look broken.
                "mail_batch_minutes",
                if old_has_batch_window {
                    "old.mail_batch_minutes".into()
                } else {
                    "5".to_string()
                },
            ),
            (
                // Same reasoning as the batch window: a database written
                // before reminders existed starts on the schema's own default
                // rather than on zero, which would read as "reminders off"
                // and silence the feature by accident.
                "reminder_minutes",
                if old_has_reminder {
                    "old.reminder_minutes".into()
                } else {
                    "15".to_string()
                },
            ),
            ("smtp_host", "old.smtp_host".into()),
            ("smtp_port", "old.smtp_port".into()),
            ("smtp_username", "old.smtp_username".into()),
            ("smtp_password", "old.smtp_password".into()),
            ("smtp_from_name", "old.smtp_from_name".into()),
            ("smtp_from_address", "old.smtp_from_address".into()),
            ("smtp_test_at", "old.smtp_test_at".into()),
            ("smtp_test_ms", "old.smtp_test_ms".into()),
            ("smtp_test_error", "old.smtp_test_error".into()),
            (
                "smtp_check_at",
                if old_has_smtp_check {
                    "old.smtp_check_at".into()
                } else {
                    "NULL".into()
                },
            ),
            (
                "smtp_check_ms",
                if old_has_smtp_check {
                    "old.smtp_check_ms".into()
                } else {
                    "NULL".into()
                },
            ),
            (
                "smtp_check_error",
                if old_has_smtp_check {
                    "old.smtp_check_error".into()
                } else {
                    "NULL".into()
                },
            ),
            ("public_url", "old.public_url".into()),
        ],
    });

    maps.push(TableMap {
        name: "user",
        columns: {
            let mut columns = old_cols(&[
                "id",
                "workspace_id",
                "email",
                "display_name",
                "role",
                "password_hash",
                "invited_by",
                "timezone",
                "theme",
                "language",
                "ui",
                "photo",
                "photo_mime",
                "created_at",
                "last_signed_in_at",
            ]);
            // The feed's read marker: new with the feed schema, so a
            // database old enough to predate it starts unseen — and a
            // database that already carries it keeps its marker across the
            // rebuild rather than having its read state wiped.
            columns.push(if old_has_feed_seen {
                ("feed_seen_at", "old.feed_seen_at".into())
            } else {
                ("feed_seen_at", "NULL".into())
            });
            columns
        },
    });
    maps.push(TableMap {
        name: "workspace_owner",
        columns: old_cols(&["singleton", "user_id", "claimed_at"]),
    });
    maps.push(TableMap {
        name: "signin_link",
        columns: {
            let mut columns = old_cols(&[
                "id",
                "user_id",
                "token_hash",
                "created_at",
                "expires_at",
                "used_at",
            ]);
            // The link's kind is new with this schema: every link a database
            // old enough to be reconciled could carry is an invitation —
            // resets did not exist to be mailed yet.
            columns.push(("kind", "'join'".into()));
            columns
        },
    });
    maps.push(TableMap {
        name: "session",
        columns: old_cols(&[
            "id",
            "user_id",
            "token_hash",
            "created_at",
            "expires_at",
            "revoked_at",
        ]),
    });
    maps.push(TableMap {
        name: "auth_attempt",
        columns: old_cols(&["id", "bucket", "attempted_at"]),
    });
    maps.push(TableMap {
        name: "board",
        columns: old_cols(&["id", "workspace_id", "name", "task_prefix", "created_at"]),
    });
    maps.push(TableMap {
        name: "board_column",
        columns: old_cols(&["id", "board_id", "name", "position", "is_done"]),
    });
    // `tag` is user data like any other table: copied with its ids intact,
    // so every `task.tag_id` keeps pointing at the tag it pointed at before.
    // The map stands even for a database from before the tag feature — the
    // copy skips it there and the seeding tops the boards up instead — so
    // the declared-schema check in `validate_maps` always sees it covered.
    maps.push(TableMap {
        name: "tag",
        columns: old_cols(&[
            "id",
            "board_id",
            "name",
            "position",
            "is_default",
            "created_at",
        ]),
    });
    maps.push(TableMap {
        name: "task",
        columns: vec![
            ("id", "old.id".into()),
            ("board_id", "old.board_id".into()),
            ("parent_id", "old.parent_id".into()),
            ("task_key", "old.task_key".into()),
            ("title", "old.title".into()),
            ("description", "old.description".into()),
            ("column_id", "old.column_id".into()),
            (
                // A database that already wears tags keeps every task's own
                // tag: the ids were copied untouched, so the reference
                // copies untouched. A database from before the tag feature
                // has no `tag_id` to copy — its tasks arrive on the board's
                // default, seeded just before this table was copied.
                "tag_id",
                if old_has_tag {
                    "old.tag_id".into()
                } else {
                    "(SELECT id FROM main.tag WHERE board_id = old.board_id AND is_default = 1)"
                        .into()
                },
            ),
            ("deadline", "old.deadline".into()),
            (
                // A clock set before this column existed never existed: the
                // task carried no meeting instant, so the old rows arrive
                // with none rather than with a made-up one.
                "clock_at",
                if old_has_clock {
                    "old.clock_at".into()
                } else {
                    "NULL".into()
                },
            ),
            ("position", "old.position".into()),
            ("created_by", "old.created_by".into()),
            ("created_at", "old.created_at".into()),
            ("updated_at", "old.updated_at".into()),
            ("done_at", "old.done_at".into()),
            ("deleted_at", "old.deleted_at".into()),
        ],
    });
    maps.push(TableMap {
        name: "task_assignee",
        columns: old_cols(&["task_id", "user_id"]),
    });
    maps.push(TableMap {
        name: "task_watcher",
        columns: old_cols(&["task_id", "user_id"]),
    });
    maps.push(TableMap {
        name: "task_dependency",
        columns: old_cols(&[
            "blocked_task_id",
            "blocking_task_id",
            "created_at",
            "cleared_at",
        ]),
    });
    maps.push(TableMap {
        name: "comment",
        columns: old_cols(&["id", "task_id", "author_id", "body", "created_at"]),
    });
    maps.push(TableMap {
        name: "attachment",
        columns: old_cols(&[
            "id",
            "task_id",
            "comment_id",
            "file_name",
            "mime_type",
            "size_bytes",
            "bytes",
            "uploaded_by",
            "created_at",
        ]),
    });
    maps.push(TableMap {
        name: "transition",
        columns: old_cols(&[
            "id",
            "task_id",
            "from_column",
            "to_column",
            "actor_id",
            "created_at",
        ]),
    });
    maps.push(TableMap {
        name: "freeing",
        columns: old_cols(&[
            "id",
            "board_id",
            "cause_key",
            "cause_title",
            "actor_id",
            "created_at",
        ]),
    });
    maps.push(TableMap {
        name: "activity",
        columns: vec![
            ("id", "old.id".into()),
            ("task_id", "old.task_id".into()),
            ("actor_id", "old.actor_id".into()),
            ("subject_id", "NULL".into()),
            ("kind", "old.kind".into()),
            ("detail", "old.detail".into()),
            ("created_at", "old.created_at".into()),
        ],
    });
    maps.push(TableMap {
        name: "mail_rule",
        columns: old_cols(&[
            "id",
            "board_id",
            "trigger_kind",
            "trigger_column",
            "subject",
            "audience",
            "enabled",
            "created_at",
            "include_task_details",
        ]),
    });
    maps.push(TableMap {
        name: "mail_send",
        columns: old_cols(&[
            "id",
            "rule_id",
            "event_id",
            "task_id",
            "recipient",
            "state",
            "attempts",
            "last_error",
            "claimed_at",
            "next_attempt_at",
            "sent_at",
            "kind",
            "subject",
            "body",
        ]),
    });
    maps.push(TableMap {
        name: "mail_decision",
        columns: old_cols(&[
            "id",
            "rule_id",
            "event_id",
            "task_id",
            "outcome",
            "detail",
            "created_at",
        ]),
    });

    maps
}

fn old_cols(cols: &[&'static str]) -> Vec<(&'static str, String)> {
    cols.iter().map(|c| (*c, format!("old.{}", c))).collect()
}

/// Returns true if the attached old `workspace` table already carries the
/// sender-check columns added by the old 0002 migration.
async fn old_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut rows = conn
        .query(&format!("PRAGMA table_info({table})"), ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let name: String = row.get(1).map_err(|e| StoreError::Backend(e.to_string()))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Validates that every destination column is covered and that no map
/// references a column that does not exist. A missing column is a startup
/// error, never a silent NULL.
async fn validate_maps(conn: &Connection, maps: &[TableMap]) -> Result<()> {
    use std::collections::HashSet;

    for map in maps {
        let sql = format!("PRAGMA table_info({})", map.name);
        let mut rows = conn
            .query(&sql, ())
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let mut actual = HashSet::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            let name: String = row.get(1).map_err(|e| StoreError::Backend(e.to_string()))?;
            actual.insert(name);
        }

        for (col, _) in &map.columns {
            if !actual.contains(*col) {
                return Err(StoreError::Backend(format!(
                    "reconcile map for {} references missing column {}",
                    map.name, col
                )));
            }
        }
        for col in actual {
            if !map.columns.iter().any(|(c, _)| c == &col) {
                return Err(StoreError::Backend(format!(
                    "reconcile map for {} is missing column {} (would become NULL)",
                    map.name, col
                )));
            }
        }
    }

    // Every table the declared schema creates must have a map: a declared
    // table without one is a table the rebuild would create empty while the
    // copy ran — the exact silence that once wiped every user tag. Engine
    // bookkeeping (`sqlite_*`) is not user data. A table the old database
    // carries but the declared schema does not is dropped on purpose; it is
    // the declared side that may never go unmapped.
    let mut rows = conn
        .query("SELECT name FROM sqlite_master WHERE type = 'table'", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let mut declared = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let name: String = row.get(0).map_err(|e| StoreError::Backend(e.to_string()))?;
        if !name.starts_with("sqlite_") {
            declared.push(name);
        }
    }
    for table in declared {
        if !maps.iter().any(|m| m.name == table) {
            return Err(StoreError::Backend(format!(
                "reconcile has no map for declared table {table} (its rows would be dropped)"
            )));
        }
    }
    Ok(())
}

/// Copies every mapped table from the attached old database into the new
/// main schema, in foreign-key-safe order.
async fn copy_data(old_conn: &Connection, new_conn: &Connection, path: &str) -> Result<()> {
    let escaped = path.replace('\'', "''");
    new_conn
        .execute(&format!("ATTACH DATABASE '{}' AS old", escaped), ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    let has_smtp_check = old_has_column(old_conn, "workspace", "smtp_check_at").await?;
    let has_batch_window = old_has_column(old_conn, "workspace", "mail_batch_minutes").await?;
    let has_clock = old_has_column(old_conn, "task", "clock_at").await?;
    let has_reminder = old_has_column(old_conn, "workspace", "reminder_minutes").await?;
    let has_feed_seen = old_has_column(old_conn, "user", "feed_seen_at").await?;
    let has_tag = old_has_column(old_conn, "tag", "id").await?;
    let maps = build_maps(
        has_smtp_check,
        has_batch_window,
        has_clock,
        has_reminder,
        has_feed_seen,
        has_tag,
    );
    validate_maps(new_conn, &maps).await?;

    for map in maps {
        // A database from before the tag feature has no `tag` table: nothing
        // to copy and nothing to lose — the seeding below stands in for it.
        // The map itself still exists, so `validate_maps` sees the declared
        // table covered whatever shape crosses the rebuild.
        if map.name == "tag" && !has_tag {
            continue;
        }
        // The default tags are seeded just before `task`: the old tags are
        // already in, so seeding only tops up the boards the copy left
        // without a default — it can therefore never violate
        // `tag_one_default` — and the task map of a pre-tag database fills
        // `tag_id` from exactly these defaults.
        if map.name == "task" {
            seed_general_tags(new_conn).await?;
        }
        let cols = map
            .columns
            .iter()
            .map(|(c, _)| *c)
            .collect::<Vec<_>>()
            .join(", ");
        // `old` is the attached SCHEMA, not a table, so `old.id` in a select
        // list reads as "table old, column id". The source table is aliased
        // and the maps' `old.` prefix rewritten onto that alias.
        let exprs = map
            .columns
            .iter()
            .map(|(_, e)| e.replace("old.", "src."))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO main.{} ({}) SELECT {} FROM old.{} AS src",
            map.name, cols, exprs, map.name
        );
        new_conn.execute(&sql, ()).await.map_err(|e| {
            StoreError::Backend(format!("{} while copying {}: {}", e, map.name, sql))
        })?;
    }

    new_conn
        .execute("DETACH DATABASE old", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    Ok(())
}

/// Verifies the rebuilt database before the swap.
async fn verify(old_conn: &Connection, new_conn: &Connection) -> Result<()> {
    let mut rows = new_conn
        .query("PRAGMA foreign_key_check", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let mut fk_errors = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let table: String = row.get(0).map_err(|e| StoreError::Backend(e.to_string()))?;
        let rowid: i64 = row.get(1).map_err(|e| StoreError::Backend(e.to_string()))?;
        let parent: String = row.get(2).map_err(|e| StoreError::Backend(e.to_string()))?;
        let fkid: i64 = row.get(3).map_err(|e| StoreError::Backend(e.to_string()))?;
        fk_errors.push(format!(
            "{} rowid {} references {} (foreign key {})",
            table, rowid, parent, fkid
        ));
    }
    if !fk_errors.is_empty() {
        return Err(StoreError::Backend(format!(
            "foreign key check failed:\n{}",
            fk_errors.join("\n")
        )));
    }

    let mut rows = new_conn
        .query("PRAGMA integrity_check", ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let mut integrity = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        let msg: String = row.get(0).map_err(|e| StoreError::Backend(e.to_string()))?;
        integrity.push(msg);
    }
    if integrity.len() != 1 || integrity[0] != "ok" {
        return Err(StoreError::Backend(format!(
            "integrity check failed: {:?}",
            integrity
        )));
    }

    let tables = [
        "workspace",
        "user",
        "workspace_owner",
        "signin_link",
        "session",
        "auth_attempt",
        "board",
        "board_column",
        "task",
        "task_assignee",
        "task_watcher",
        "task_dependency",
        "comment",
        "attachment",
        "transition",
        "freeing",
        "activity",
        "mail_rule",
        "mail_send",
        "mail_decision",
    ];
    for table in tables {
        let old_count = count_rows(old_conn, table).await?;
        let new_count = count_rows(new_conn, table).await?;
        if old_count != new_count {
            return Err(StoreError::Backend(format!(
                "row count mismatch for {}: old {} new {}",
                table, old_count, new_count
            )));
        }
    }

    // must be here row for row — same id, board, name, order, default mark,
    // creation instant — and anything new must be a default the seeding put
    // there for a board that came across with none. This is the strongest
    // cheap check: a full row comparison rather than a count, so a rebuild
    // that dropped, renumbered or rewrote a single tag fails here instead of
    // shipping.
    let old_has_tag = old_has_column(old_conn, "tag", "id").await?;
    let old_tags = if old_has_tag {
        read_tag_rows(old_conn).await?
    } else {
        Vec::new()
    };
    let new_tags = read_tag_rows(new_conn).await?;
    for lost in &old_tags {
        if !new_tags.contains(lost) {
            return Err(StoreError::Backend(format!(
                "tag {} ({}) did not survive the rebuild unchanged",
                lost.0, lost.2
            )));
        }
    }
    for extra in &new_tags {
        if !old_tags.contains(extra) && (extra.4 != 1 || extra.2 != "General") {
            return Err(StoreError::Backend(format!(
                "tag {} ({}) appeared in the rebuild without being a seeded default",
                extra.0, extra.2
            )));
        }
    }

    Ok(())
}

/// Reads every tag row as plain values, so the verification can compare the
/// old and the new table row for row.
async fn read_tag_rows(
    conn: &Connection,
) -> Result<Vec<(String, String, String, i64, i64, String)>> {
    let mut rows = conn
        .query(
            "SELECT id, board_id, name, position, is_default, created_at FROM tag",
            (),
        )
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let mut tags = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    {
        tags.push((
            row.get::<String>(0)
                .map_err(|e| StoreError::Backend(e.to_string()))?,
            row.get::<String>(1)
                .map_err(|e| StoreError::Backend(e.to_string()))?,
            row.get::<String>(2)
                .map_err(|e| StoreError::Backend(e.to_string()))?,
            row.get::<i64>(3)
                .map_err(|e| StoreError::Backend(e.to_string()))?,
            row.get::<i64>(4)
                .map_err(|e| StoreError::Backend(e.to_string()))?,
            row.get::<String>(5)
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        ));
    }
    Ok(tags)
}

async fn count_rows(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {}", table);
    let mut rows = conn
        .query(&sql, ())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let Some(row) = rows
        .next()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?
    else {
        return Err(StoreError::Backend(format!(
            "could not count rows in {}",
            table
        )));
    };
    row.get::<i64>(0)
        .map_err(|e| StoreError::Backend(e.to_string()))
}

fn cleanup_rebuilt(rebuilt_path: &str) {
    let _ = std::fs::remove_file(rebuilt_path);
    let _ = std::fs::remove_file(format!("{}-wal", rebuilt_path));
    let _ = std::fs::remove_file(format!("{}-shm", rebuilt_path));
}

/// Move a SQLite sidecar file (`-wal` or `-shm`) if it exists. A missing
/// sibling is fine: WAL mode may not have created one, and the backup is
/// still consistent without it.
fn rename_sibling(from: &str, to: &str, suffix: &str) {
    let from_path = format!("{}{}", from, suffix);
    let to_path = format!("{}{}", to, suffix);
    if std::path::Path::new(&from_path).exists() {
        let _ = std::fs::rename(&from_path, &to_path);
    }
}

fn backup_name(path: &str) -> Result<String> {
    let format = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]T[hour][minute][second]Z",
    )
    .map_err(|e| StoreError::Backend(format!("invalid time format: {}", e)))?;
    let stamp = time::OffsetDateTime::now_utc()
        .format(&format)
        .map_err(|e| StoreError::Backend(format!("time format failed: {}", e)))?;

    let candidate = format!("{}.backup-{}", path, stamp);
    if !std::path::Path::new(&candidate).exists() {
        return Ok(candidate);
    }
    let mut n = 1;
    loop {
        let candidate = format!("{}.backup-{}-{}", path, stamp, n);
        if !std::path::Path::new(&candidate).exists() {
            return Ok(candidate);
        }
        n += 1;
        if n > 1000 {
            return Err(StoreError::Backend(
                "could not find an unused backup name".into(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TableMap, backup_name, build_maps, validate_maps};
    use turso::Builder;
    use ulid::Ulid;
    use crate::store::schema::SCHEMA;

    #[test]
    fn backup_name_has_no_colons_or_spaces() {
        let name = backup_name("/tmp/izlek.db").unwrap();
        assert!(name.starts_with("/tmp/izlek.db.backup-"));
        assert!(!name.contains(':'));
        assert!(!name.contains(' '));
    }

    #[test]
    fn backup_name_avoids_collision() {
        let first = backup_name("/tmp/izlek.db").unwrap();
        std::fs::File::create(&first).unwrap();
        let second = backup_name("/tmp/izlek.db").unwrap();
        assert_ne!(first, second);
        std::fs::remove_file(&first).unwrap();
    }

    #[tokio::test]
    async fn validate_maps_refuses_a_declared_table_without_a_map() {
        let dir = std::env::temp_dir().join(format!("izlek-validate-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("izlek.db").to_str().unwrap().to_string();
        let db = Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch(SCHEMA).await.unwrap();

        let maps = build_maps(false, false, false, false, false, false);
        validate_maps(&conn, &maps)
            .await
            .expect("the full map set was refused against the declared schema");

        // Drop any one table's map — here the tags, the table the maps once
        // silently wiped — and the guard must name the table rather than let
        // the rebuild create it empty.
        let maps: Vec<TableMap> = maps.into_iter().filter(|m| m.name != "tag").collect();
        let err = validate_maps(&conn, &maps)
            .await
            .expect_err("an unmapped declared table passed validation");
        assert!(
            err.to_string().contains("tag"),
            "the error does not name the unmapped table: {err}"
        );

        drop(conn);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

}
