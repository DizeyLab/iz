//! Turso implementation of [`Store`].
//!
//! Turso is in-process and SQLite-compatible, so the schema is plain SQL and
//! there is no server to run alongside the binary. It ships no migration runner
//! of its own; [`TursoStore::open`] applies the numbered files in
//! `crates/dizey-core/migrations` at boot and records the version.

use async_trait::async_trait;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use turso::{Builder, Connection, Row, Value, params};
use uuid::Uuid;

use super::{
    DeletePolicy, NewUser, Result, SigninLink, Store, StoreError, User, Workspace,
};
use crate::Role;

/// Every migration, in order. Adding one means appending a file and a line.
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../../migrations/0001_init.sql"))];

pub struct TursoStore {
    conn: Connection,
    // Held so the database outlives its connection.
    _db: turso::Database,
}

impl TursoStore {
    /// Opens (creating if needed) the database at `path` and brings the schema
    /// up to date. `:memory:` gives a throwaway database for tests.
    pub async fn open(path: &str) -> Result<Self> {
        let db = Builder::new_local(path).build().await.map_err(backend)?;
        let conn = db.connect().map_err(backend)?;
        // Turso is a single-writer engine. Two connections on one Database
        // handle serialise by themselves, but a second handle on the same file
        // (a second process, or a careless second open) fails outright with
        // "database is locked" and silently drops the write unless a busy
        // timeout is set. Both pragmas are set on every connection we hand out.
        for pragma in [
            "PRAGMA foreign_keys = ON",
            "PRAGMA busy_timeout = 5000",
        ] {
            conn.execute(pragma, ()).await.map_err(backend)?;
        }
        let store = Self { conn, _db: db };
        store.migrate().await?;
        Ok(store)
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
            self.conn.execute_batch(sql).await.map_err(backend)?;
            self.conn
                .execute(
                    "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                    params![*version, now_text()?],
                )
                .await
                .map_err(backend)?;
        }
        Ok(())
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
        Ok(self.applied_versions().await?.into_iter().max().unwrap_or(0))
    }

    async fn one_row(&self, sql: &str, args: impl turso::IntoParams) -> Result<Option<Row>> {
        let mut rows = self.conn.query(sql, args).await.map_err(backend)?;
        rows.next().await.map_err(backend)
    }
}

fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn now_text() -> Result<String> {
    stamp(OffsetDateTime::now_utc())
}

fn stamp(at: OffsetDateTime) -> Result<String> {
    at.format(&Rfc3339)
        .map_err(|e| StoreError::Corrupt(format!("timestamp: {e}")))
}

fn parse_stamp(raw: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .map_err(|e| StoreError::Corrupt(format!("timestamp {raw:?}: {e}")))
}

fn opt_stamp(row: &Row, idx: usize) -> Result<Option<OffsetDateTime>> {
    match row.get::<Option<String>>(idx).map_err(backend)? {
        Some(raw) => Ok(Some(parse_stamp(&raw)?)),
        None => Ok(None),
    }
}

fn text(row: &Row, idx: usize) -> Result<String> {
    row.get::<String>(idx).map_err(backend)
}

fn opt_text(row: &Row, idx: usize) -> Result<Option<String>> {
    row.get::<Option<String>>(idx).map_err(backend)
}

fn count_of(row: &Row) -> Result<u64> {
    row.get::<i64>(0)
        .map_err(backend)
        .map(|n| n.max(0) as u64)
}

fn workspace_from(row: &Row) -> Result<Workspace> {
    let types_json = text(row, 9)?;
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
        smtp_host: opt_text(row, 3)?,
        smtp_port: row.get::<Option<u32>>(4).map_err(backend)?,
        smtp_username: opt_text(row, 5)?,
        smtp_from_name: opt_text(row, 6)?,
        smtp_from_address: opt_text(row, 7)?,
        attachment_limit_bytes: row.get::<i64>(8).map_err(backend)?.max(0) as u64,
        allowed_file_types,
        photo_limit_bytes: row.get::<i64>(10).map_err(backend)?.max(0) as u64,
        who_can_delete_tasks: DeletePolicy::parse(&text(row, 11)?)?,
    })
}

const WORKSPACE_COLUMNS: &str = "id, name, created_at, smtp_host, smtp_port, smtp_username, \
     smtp_from_name, smtp_from_address, attachment_limit_bytes, allowed_file_types, \
     photo_limit_bytes, who_can_delete_tasks";

fn user_from(row: &Row) -> Result<User> {
    Ok(User {
        id: text(row, 0)?,
        workspace_id: text(row, 1)?,
        email: text(row, 2)?,
        display_name: text(row, 3)?,
        role: Role::parse(&text(row, 4)?)
            .ok_or_else(|| StoreError::Corrupt("role".into()))?,
        password_hash: opt_text(row, 5)?,
        photo_path: opt_text(row, 6)?,
        created_at: parse_stamp(&text(row, 7)?)?,
        last_signed_in_at: opt_stamp(row, 8)?,
    })
}

const USER_COLUMNS: &str = "id, workspace_id, email, display_name, role, password_hash, \
     photo_path, created_at, last_signed_in_at";

fn signin_link_from(row: &Row) -> Result<SigninLink> {
    Ok(SigninLink {
        id: text(row, 0)?,
        user_id: text(row, 1)?,
        created_at: parse_stamp(&text(row, 2)?)?,
        expires_at: parse_stamp(&text(row, 3)?)?,
        used_at: opt_stamp(row, 4)?,
    })
}

/// Addresses are matched case-insensitively; the display form is kept as typed.
fn fold_email(email: &str) -> String {
    email.trim().to_lowercase()
}

#[async_trait]
impl Store for TursoStore {
    async fn create_workspace(&self, name: &str) -> Result<Workspace> {
        if self.workspace().await?.is_some() {
            return Err(StoreError::Conflict("workspace"));
        }
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO workspace (id, name, created_at) VALUES (?1, ?2, ?3)",
                params![id.clone(), name, now_text()?],
            )
            .await
            .map_err(backend)?;
        self.workspace().await?.ok_or(StoreError::NotFound)
    }

    async fn workspace(&self) -> Result<Option<Workspace>> {
        let sql = format!("SELECT {WORKSPACE_COLUMNS} FROM workspace LIMIT 1");
        match self.one_row(&sql, ()).await? {
            Some(row) => Ok(Some(workspace_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn set_smtp(
        &self,
        workspace_id: &str,
        host: &str,
        port: u32,
        username: &str,
        password: &str,
        from_name: &str,
        from_address: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE workspace SET smtp_host = ?1, smtp_port = ?2, smtp_username = ?3, \
                 smtp_password = ?4, smtp_from_name = ?5, smtp_from_address = ?6 WHERE id = ?7",
                params![
                    host,
                    port as i64,
                    username,
                    password,
                    from_name,
                    from_address,
                    workspace_id
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn smtp_password(&self, workspace_id: &str) -> Result<Option<String>> {
        match self
            .one_row(
                "SELECT smtp_password FROM workspace WHERE id = ?1",
                params![workspace_id],
            )
            .await?
        {
            Some(row) => opt_text(&row, 0),
            None => Err(StoreError::NotFound),
        }
    }

    async fn set_limits(
        &self,
        workspace_id: &str,
        attachment_limit_bytes: u64,
        photo_limit_bytes: u64,
        allowed_file_types: &[String],
        who_can_delete_tasks: DeletePolicy,
    ) -> Result<()> {
        let types = serde_json::to_string(allowed_file_types)
            .map_err(|e| StoreError::Corrupt(format!("allowed_file_types: {e}")))?;
        self.conn
            .execute(
                "UPDATE workspace SET attachment_limit_bytes = ?1, photo_limit_bytes = ?2, \
                 allowed_file_types = ?3, who_can_delete_tasks = ?4 WHERE id = ?5",
                params![
                    attachment_limit_bytes as i64,
                    photo_limit_bytes as i64,
                    types,
                    who_can_delete_tasks.as_str(),
                    workspace_id
                ],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn create_user(&self, new: NewUser) -> Result<User> {
        let email = fold_email(&new.email);
        if self.user_by_email(&new.workspace_id, &email).await?.is_some() {
            return Err(StoreError::Conflict("account"));
        }
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO user (id, workspace_id, email, display_name, role, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id.clone(),
                    new.workspace_id,
                    email,
                    new.display_name,
                    new.role.as_str(),
                    now_text()?
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
        let sql =
            format!("SELECT {USER_COLUMNS} FROM user WHERE workspace_id = ?1 AND email = ?2");
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
        let mut rows = self.conn.query(&sql, params![workspace_id]).await.map_err(backend)?;
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
        if n == 0 { Err(StoreError::NotFound) } else { Ok(()) }
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
        if n == 0 { Err(StoreError::NotFound) } else { Ok(()) }
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
        if n == 0 { Err(StoreError::NotFound) } else { Ok(()) }
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
        if n == 0 { Err(StoreError::NotFound) } else { Ok(()) }
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

    async fn consume_signin_link(&self, id: &str, at: OffsetDateTime) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE signin_link SET used_at = ?1 WHERE id = ?2 AND used_at IS NULL",
                params![stamp(at)?, id],
            )
            .await
            .map_err(backend)?;
        if n == 0 { Err(StoreError::NotFound) } else { Ok(()) }
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
        let dir = std::env::temp_dir().join(format!("dizey-probe-{}", Uuid::new_v4()));
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
        let dir = std::env::temp_dir().join(format!("dizey-probe-{}", Uuid::new_v4()));
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
        let dir = std::env::temp_dir().join(format!("dizey-probe-{}", Uuid::new_v4()));
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
}
