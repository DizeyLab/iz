//! The account surface: claiming the workspace, first sign-in from an invited
//! link, signing in and out, and changing a password.
//!
//! The public forms answer identically whether or not an address has an
//! account. The honest, specific wording belongs to the admin-side calls.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

/// Everything a refused call is allowed to say.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// Wrong address, wrong password, or no account at all — deliberately one
    /// answer for all three.
    Rejected,
    RateLimited,
    /// A password broke a stated rule. Only reachable once we know who the
    /// person is, so it gives nothing away.
    Password(String),
    Forbidden,
    SignInFirst,
    AlreadyClaimed,
    /// Admin-side wording, shown on the member list and nowhere public.
    AddressTaken,
    /// The link is spent, expired, or was never real.
    LinkNotUsable,
    /// A card with no title is not a card.
    EmptyTitle,
    Unavailable,
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::Rejected => "That did not work.".to_string(),
            Refusal::RateLimited => {
                "Too many attempts — wait a few minutes and try again.".to_string()
            }
            Refusal::Password(problem) => problem.clone(),
            Refusal::Forbidden => "Not permitted.".to_string(),
            Refusal::SignInFirst => "Sign in first.".to_string(),
            Refusal::AlreadyClaimed => "This workspace already has an owner.".to_string(),
            Refusal::AddressTaken => "That address already has an account.".to_string(),
            Refusal::LinkNotUsable => {
                "This link no longer works. Ask the admin to send another."
                    .to_string()
            }
            Refusal::EmptyTitle => "Give the task a title.".to_string(),
            Refusal::Unavailable => "Something went wrong. Try again.".to_string(),
        }
    }
}

/// The person the current browser is signed in as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// The account id, so the board can tell "Mine" from everyone else's
    /// without a second call.
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub role: dizey_core::Role,
}

/// Which of the three doors this browser is standing at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Gate {
    /// Nobody has signed in yet: the next account administers the workspace.
    NeedsSetup,
    NeedsSignIn,
    SignedIn(Me),
}

/// The name and address an invitation was made out to. Only the holder of the
/// link can ask, and the link is a 128-bit secret, so answering is safe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invited {
    pub display_name: String,
    pub email: String,
}

/// The workspace has exactly one name and no screen that sets it, so it is a
/// constant rather than a field nobody was shown.
pub const WORKSPACE_NAME: &str = "Dizey";

/// Which door to show. (The `#[server]` macro names a struct after the
/// function, so the call is `current_gate` and the answer is a `Gate`.)
#[server]
pub async fn current_gate() -> Result<Gate, ServerFnError> {
    use crate::server::{accounts, current_user};

    if let Some(user) = current_user().await {
        return Ok(Gate::SignedIn(Me {
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        }));
    }
    let claimed = accounts()
        .store()
        .owner()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .is_some();
    Ok(if claimed {
        Gate::NeedsSignIn
    } else {
        Gate::NeedsSetup
    })
}

/// The first account. It becomes the admin and owns the workspace.
#[server]
pub async fn claim_workspace(
    display_name: String,
    email: String,
    password: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, set_session_cookie};
    use dizey_core::accounts::SESSION_LIFETIME;

    match accounts()
        .claim_workspace(WORKSPACE_NAME, &email, &display_name, &password)
        .await
    {
        Ok((_workspace, signed_in)) => {
            set_session_cookie(signed_in.session_token.expose(), SESSION_LIFETIME);
            Ok(None)
        }
        Err(error) => Ok(Some(error.into())),
    }
}

/// Who an invitation was made out to, for the "signing in as" line.
#[server]
pub async fn invitation(token: String) -> Result<Option<Invited>, ServerFnError> {
    use crate::server::accounts;
    use dizey_core::auth::hash_token;
    use time::OffsetDateTime;

    let store = accounts().store().clone();
    let digest = hash_token(&token);
    let Some(link) = store
        .signin_link_by_hash(&digest)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
    else {
        return Ok(None);
    };
    if !link.is_usable(OffsetDateTime::now_utc()) {
        return Ok(None);
    }
    let Some(user) = store
        .user(&link.user_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
    else {
        return Ok(None);
    };
    Ok(Some(Invited {
        display_name: user.display_name,
        email: user.email,
    }))
}

/// The invited member's first sign-in: they pick their own password. The admin
/// can neither read nor set it.
#[server]
pub async fn redeem_link(
    token: String,
    password: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, client_label, set_session_cookie};
    use dizey_core::accounts::SESSION_LIFETIME;

    match accounts()
        .redeem_signin_link(&token, &password, &client_label())
        .await
    {
        Ok(signed_in) => {
            set_session_cookie(signed_in.session_token.expose(), SESSION_LIFETIME);
            Ok(None)
        }
        Err(error) => Ok(Some(error.into())),
    }
}

/// Signing in. Answers the same whether the address is unknown, has no password
/// yet, or the password is wrong.
#[server]
pub async fn sign_in(
    email: String,
    password: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, client_label, set_session_cookie};
    use dizey_core::accounts::SESSION_LIFETIME;

    match accounts()
        .sign_in(&email, &password, &client_label())
        .await
    {
        Ok(signed_in) => {
            set_session_cookie(signed_in.session_token.expose(), SESSION_LIFETIME);
            Ok(None)
        }
        Err(error) => Ok(Some(error.into())),
    }
}

/// Ends this browser's session. Other browsers keep theirs.
#[server]
pub async fn sign_out() -> Result<(), ServerFnError> {
    use crate::server::{accounts, clear_session_cookie, presented_session};

    if let Some(presented) = presented_session() {
        let _ = accounts().sign_out(&presented).await;
    }
    clear_session_cookie();
    Ok(())
}

/// Changes the password and signs the other devices out, as the pane promises.
/// The browser that asked gets a fresh cookie.
#[server]
pub async fn change_password(
    current: String,
    new: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{client_label, require_user, set_session_cookie};
    use dizey_core::accounts::SESSION_LIFETIME;

    let user = match require_user().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    match crate::server::accounts()
        .change_password(&user.id, &current, &new, &client_label())
        .await
    {
        Ok(signed_in) => {
            set_session_cookie(signed_in.session_token.expose(), SESSION_LIFETIME);
            Ok(None)
        }
        Err(error) => Ok(Some(error.into())),
    }
}

/// Admin creates an account with a name and an address and no password, and
/// gets the first-sign-in link back exactly once.
#[server]
pub async fn invite_member(
    email: String,
    display_name: String,
    role: dizey_core::Role,
) -> Result<Result<String, Refusal>, ServerFnError> {
    use crate::server::{accounts, require_admin};

    let admin = match require_admin().await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(Err(refusal)),
    };
    match accounts().invite(&admin, &email, &display_name, role).await {
        Ok(invitation) => Ok(Ok(format!("/join/{}", invitation.token.expose()))),
        Err(error) => Ok(Err(error.into())),
    }
}
