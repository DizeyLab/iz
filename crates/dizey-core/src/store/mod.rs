//! Storage boundary.
//!
//! Everything the app does to persistent state goes through [`Store`]. The only
//! implementation today is Turso (in-process, SQLite-compatible); a Postgres
//! swap is a new impl of this trait and nothing else.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::Role;

pub mod turso_store;

pub use turso_store::TursoStore;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Backend(String),
    #[error("not found")]
    NotFound,
    #[error("{0} already exists")]
    Conflict(&'static str),
    #[error("stored value is not valid: {0}")]
    Corrupt(String),
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

/// The storage boundary. Dyn-safe on purpose: handlers hold `Arc<dyn Store>`.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    // -- workspace ---------------------------------------------------------

    /// Creates the workspace. Fails with [`StoreError::Conflict`] if one
    /// already exists: Dizey hosts a single workspace per database.
    async fn create_workspace(&self, name: &str) -> Result<Workspace>;

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

    async fn consume_signin_link(&self, id: &str, at: OffsetDateTime) -> Result<()>;
}
