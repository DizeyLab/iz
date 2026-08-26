//! The account flows: claiming the workspace, inviting people, first sign-in,
//! signing in, changing a password, and turning a cookie back into a person.
//!
//! Two rules run through all of it.
//!
//! * **The public surface answers the same way whether or not an address has an
//!   account.** That means the same wording *and* the same work: every path
//!   that misses still pays for an Argon2 verify, or the response time becomes
//!   the oracle the wording refuses to be. The honest, specific strings in the
//!   mockups are admin-side copy and stay admin-side.
//! * **Capability checks live here, not in the UI.** A Viewer or a Member
//!   calling an admin-only flow is rejected by this module.

use std::sync::Arc;

use time::{Duration, OffsetDateTime};

use crate::Role;
use crate::auth::{self, AuthError, PasswordProblem, Token, hash_password, verify_password};
use crate::store::{NewUser, Session, Store, StoreError, User, Workspace};

/// How long a first-sign-in link is good for. The mockups say seven days, and
/// an expired link is not a dead account: resending opens the same one.
pub const SIGNIN_LINK_LIFETIME: Duration = Duration::days(7);

/// How long a signed-in browser stays signed in.
pub const SESSION_LIFETIME: Duration = Duration::days(14);

/// The rate-limit window and its allowance. Deliberately modest: this exists so
/// an open form is not an unbounded Argon2 faucet, not to be clever.
pub const RATE_WINDOW: Duration = Duration::minutes(15);
pub const RATE_LIMIT: u64 = 10;

/// What went wrong, in the vocabulary the caller is allowed to have.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// The credentials did not work. Deliberately says nothing about which
    /// half, or whether the address exists at all.
    #[error("that did not work")]
    Rejected,
    /// Too many attempts from this address or this client, recently.
    #[error("too many attempts — wait a few minutes and try again")]
    RateLimited,
    /// A password the person chose broke a stated rule. Only ever returned on
    /// screens where we already know who they are.
    #[error("{0}")]
    Password(#[from] PasswordProblem),
    /// The caller's role does not permit this.
    #[error("not permitted")]
    Forbidden,
    /// This workspace already has an owner.
    #[error("this workspace already has an owner")]
    AlreadyClaimed,
    /// That address already has an account here. Admin-side wording: it is
    /// shown on the member list, never on a public form.
    #[error("that address already has an account")]
    AddressTaken,
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Auth(#[from] AuthError),
}

pub type Result<T> = std::result::Result<T, AccountError>;

/// A person plus the browser they are using.
#[derive(Debug)]
pub struct SignedIn {
    pub user: User,
    /// The cookie value. Exists exactly once, here.
    pub session_token: Token,
    pub session: Session,
}

/// An invitation the admin can hand over. The link is shown once; after that
/// only its hash exists.
#[derive(Debug)]
pub struct Invitation {
    pub user: User,
    pub token: Token,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone)]
pub struct Accounts {
    store: Arc<dyn Store>,
}

impl Accounts {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    // -- claiming ----------------------------------------------------------

    /// The very first account. It becomes the admin and owns the workspace.
    ///
    /// The claim is atomic in the store, so two people racing an empty install
    /// cannot both end up admin and the loser does not quietly become a member.
    pub async fn claim_workspace(
        &self,
        workspace_name: &str,
        email: &str,
        display_name: &str,
        password: &str,
    ) -> Result<(Workspace, SignedIn)> {
        auth::check_password(password, email, display_name)?;
        let hash = hash_password(password)?;
        let (workspace, admin) = match self
            .store
            .claim_workspace(workspace_name, email, display_name, &hash)
            .await
        {
            Ok(pair) => pair,
            Err(StoreError::AlreadyClaimed) => return Err(AccountError::AlreadyClaimed),
            Err(e) => return Err(e.into()),
        };
        let signed_in = self.start_session(admin).await?;
        Ok((workspace, signed_in))
    }

    // -- inviting ----------------------------------------------------------

    /// Creates an account with a name and an address and no password, and mints
    /// the link that lets that person choose one.
    ///
    /// The admin never learns the password: only the hash of this link is
    /// stored, and the password is set by the person on the other end.
    pub async fn invite(
        &self,
        actor: &User,
        email: &str,
        display_name: &str,
        role: Role,
    ) -> Result<Invitation> {
        self.require_admin(actor)?;
        let user = self
            .store
            .create_user(NewUser {
                workspace_id: actor.workspace_id.clone(),
                email: email.to_string(),
                display_name: display_name.to_string(),
                role,
                invited_by: Some(actor.id.clone()),
            })
            .await
            .map_err(|e| match e {
                StoreError::Conflict("account") => AccountError::AddressTaken,
                other => other.into(),
            })?;
        self.mint_link(actor, &user).await
    }

    /// Sends the same person a fresh link. An expired link is not a dead
    /// account: this opens the same one.
    pub async fn resend_invitation(&self, actor: &User, user_id: &str) -> Result<Invitation> {
        self.require_admin(actor)?;
        let user = self
            .store
            .user(user_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if user.workspace_id != actor.workspace_id {
            return Err(AccountError::Forbidden);
        }
        self.mint_link(actor, &user).await
    }

    async fn mint_link(&self, _actor: &User, user: &User) -> Result<Invitation> {
        let token = Token::mint();
        let expires_at = OffsetDateTime::now_utc() + SIGNIN_LINK_LIFETIME;
        self.store
            .create_signin_link(&user.id, &token.hash(), expires_at)
            .await?;
        Ok(Invitation {
            user: user.clone(),
            token,
            expires_at,
        })
    }

    // -- first sign-in -----------------------------------------------------

    /// Redeems a first-sign-in link and sets the password the person chose.
    ///
    /// Every failure — unknown link, expired link, already-used link — is the
    /// same [`AccountError::Rejected`], and the miss path pays for a hash so
    /// the timing says nothing either.
    ///
    /// The limit is bucketed on the client, not on the presented token: a
    /// bucket keyed on the token would be fresh for every guess and would never
    /// catch anyone walking the token space, while every miss here pays for a
    /// full Argon2 hash.
    pub async fn redeem_signin_link(
        &self,
        presented: &str,
        password: &str,
        client: &str,
    ) -> Result<SignedIn> {
        let now = OffsetDateTime::now_utc();
        let client_bucket = format!("client:{client}");
        self.check_rate(&client_bucket).await?;
        self.store.record_auth_attempt(&client_bucket, now).await?;
        let digest = auth::hash_token(presented);

        let link = match self.store.signin_link_by_hash(&digest).await? {
            Some(link) => link,
            None => {
                // Same work as the success path, minus the effect.
                let _ = hash_password(password);
                return Err(AccountError::Rejected);
            }
        };
        // Expiry is decided here, against the stored timestamp. Nothing the
        // client sends is consulted.
        if !link.is_usable(now) {
            let _ = hash_password(password);
            return Err(AccountError::Rejected);
        }
        let user = match self.store.user(&link.user_id).await? {
            Some(user) => user,
            None => {
                let _ = hash_password(password);
                return Err(AccountError::Rejected);
            }
        };

        // The rules are checked before the link is spent, so a rejected
        // password does not burn the invitation.
        auth::check_password(password, &user.email, &user.display_name)?;
        let hash = hash_password(password)?;

        // The atomic step. A prefetching mail client and the person clicking
        // arrive together; exactly one of them consumes the link.
        if !self.store.consume_signin_link(&link.id, now).await? {
            return Err(AccountError::Rejected);
        }

        self.store.set_password_hash(&user.id, &hash).await?;
        self.store.clear_auth_attempts(&client_bucket).await?;
        let user = self
            .store
            .user(&user.id)
            .await?
            .ok_or(StoreError::NotFound)?;
        self.start_session(user).await
    }

    // -- signing in --------------------------------------------------------

    /// Signs a person in. `client` is whatever identifies the caller's machine
    /// — it only ever becomes a rate-limit bucket.
    pub async fn sign_in(&self, email: &str, password: &str, client: &str) -> Result<SignedIn> {
        let client_bucket = format!("client:{client}");
        // Checked before anything else, including the pre-claim path below,
        // which would otherwise be an unmetered dummy-verify.
        self.check_rate(&client_bucket).await?;
        let now = OffsetDateTime::now_utc();
        self.store.record_auth_attempt(&client_bucket, now).await?;

        let workspace = match self.store.workspace().await? {
            Some(workspace) => workspace,
            None => {
                auth::dummy_verify(password);
                return Err(AccountError::Rejected);
            }
        };
        let address_bucket = format!("address:{}", email.trim().to_lowercase());
        // The address bucket counts, but it never refuses on its own: refusing
        // before the verify would let anyone who knows a colleague's address
        // lock them out with ten wrong guesses every fifteen minutes. A correct
        // password always gets in; a wrong one still pays for its Argon2 and
        // still counts. The client bucket is what caps the work.
        let address_over_limit = self.over_limit(&address_bucket).await?;
        self.store.record_auth_attempt(&address_bucket, now).await?;

        let user = self.store.user_by_email(&workspace.id, email).await?;
        let hash = user.as_ref().and_then(|u| u.password_hash.clone());
        // Three misses share one path: no such address, an invited account that
        // has not set a password yet, and a wrong password. Each pays for one
        // Argon2 verify and gets the same answer.
        let ok = match hash.as_deref() {
            Some(stored) => verify_password(password, stored),
            None => {
                auth::dummy_verify(password);
                false
            }
        };
        if !ok {
            return Err(if address_over_limit {
                AccountError::RateLimited
            } else {
                AccountError::Rejected
            });
        }

        let user = user.expect("a hash implies a user");
        self.store.clear_auth_attempts(&address_bucket).await?;
        self.store.clear_auth_attempts(&client_bucket).await?;
        self.store.mark_signed_in(&user.id, now).await?;
        self.start_session(user).await
    }

    /// Changes a password and signs every browser out, including the one that
    /// asked — the caller gets a fresh session token back.
    pub async fn change_password(
        &self,
        user_id: &str,
        current: &str,
        new: &str,
        client: &str,
    ) -> Result<SignedIn> {
        // A signed-in browser someone walked away from is still an Argon2
        // faucet and a guessing oracle on the current password.
        let client_bucket = format!("client:{client}");
        self.check_rate(&client_bucket).await?;
        self.store
            .record_auth_attempt(&client_bucket, OffsetDateTime::now_utc())
            .await?;
        let user = self
            .store
            .user(user_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        let ok = match user.password_hash.as_deref() {
            Some(stored) => verify_password(current, stored),
            None => {
                auth::dummy_verify(current);
                false
            }
        };
        if !ok {
            return Err(AccountError::Rejected);
        }
        auth::check_password(new, &user.email, &user.display_name)?;
        let hash = hash_password(new)?;
        self.store.set_password_hash(&user.id, &hash).await?;
        self.store.clear_auth_attempts(&client_bucket).await?;
        // "Signs out your other devices", as the pane promises.
        self.store
            .revoke_sessions_for_user(&user.id, OffsetDateTime::now_utc())
            .await?;
        let user = self
            .store
            .user(&user.id)
            .await?
            .ok_or(StoreError::NotFound)?;
        self.start_session(user).await
    }

    // -- sessions ----------------------------------------------------------

    /// Turns a cookie value back into a person, or into nothing at all.
    pub async fn authenticate(&self, presented: &str) -> Result<Option<User>> {
        let digest = auth::hash_token(presented);
        let Some(session) = self.store.session_by_hash(&digest).await? else {
            return Ok(None);
        };
        // The index found it; compare the digests properly before trusting it.
        let Some(stored) = self.store.session_token_hash(&session.id).await? else {
            return Ok(None);
        };
        if !auth::digests_match(&stored, &digest) {
            return Ok(None);
        }
        if !session.is_live(OffsetDateTime::now_utc()) {
            return Ok(None);
        }
        Ok(self.store.user(&session.user_id).await?)
    }

    /// Ends one browser's session.
    pub async fn sign_out(&self, presented: &str) -> Result<()> {
        let digest = auth::hash_token(presented);
        if let Some(session) = self.store.session_by_hash(&digest).await? {
            let _ = self
                .store
                .revoke_session(&session.id, OffsetDateTime::now_utc())
                .await;
        }
        Ok(())
    }

    async fn start_session(&self, user: User) -> Result<SignedIn> {
        let token = Token::mint();
        let session = self
            .store
            .create_session(
                &user.id,
                &token.hash(),
                OffsetDateTime::now_utc() + SESSION_LIFETIME,
            )
            .await?;
        Ok(SignedIn {
            user,
            session_token: token,
            session,
        })
    }

    // -- guards ------------------------------------------------------------

    fn require_admin(&self, actor: &User) -> Result<()> {
        if actor.role.can_administer() {
            Ok(())
        } else {
            Err(AccountError::Forbidden)
        }
    }

    async fn check_rate(&self, bucket: &str) -> Result<()> {
        if self.over_limit(bucket).await? {
            return Err(AccountError::RateLimited);
        }
        Ok(())
    }

    /// The same question without the refusal, for the bucket that counts but
    /// must not lock an account out.
    async fn over_limit(&self, bucket: &str) -> Result<bool> {
        let since = OffsetDateTime::now_utc() - RATE_WINDOW;
        Ok(self.store.count_auth_attempts(bucket, since).await? >= RATE_LIMIT)
    }
}
