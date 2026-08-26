//! The account surface: claiming the workspace, first sign-in from an invited
//! link, signing in and out, and changing a password.
//!
//! The public forms answer identically whether or not an address has an
//! account. The honest, specific wording belongs to the admin-side calls.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

/// The two password wordings, as the store states them. They are repeated here
/// because a password problem has to be named on the client — which has no
/// `dizey_core::auth` — and `the_password_wordings_match_the_store` below fails
/// the build if the two ever drift apart.
const TOO_SHORT: &str = "at least 10 characters";
const LOOKS_LIKE_YOU: &str = "not your address or your name";

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
    /// A person with no name is not a person the board can show.
    EmptyName,
    /// A limit of nothing, or a limit wider than the disk should promise.
    BadLimit,
    /// Something in the allowed-types list is not a file extension.
    BadFileType,
    /// A comment with nothing in it.
    EmptyComment,
    /// The date field did not hold a date.
    BadDeadline,
    /// The link asked for would put a task behind itself.
    Cycle,
    /// The card was already moved out of the column this request thought it
    /// was in, by somebody else, while this person was deciding.
    MovedAlready,
    /// No such task — or none this account may see. Deliberately one answer for
    /// both.
    NotFound,
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
                "This link no longer works. Ask the admin to send another.".to_string()
            }
            Refusal::EmptyTitle => "Give the task a title.".to_string(),
            Refusal::EmptyName => "Give yourself a name.".to_string(),
            Refusal::BadLimit => {
                "A limit has to be at least 1 MB, and no wider than 500 MB per file or 20 MB per photo."
                    .to_string()
            }
            Refusal::BadFileType => {
                "File types are extensions — png, pdf, zip — separated by commas.".to_string()
            }
            Refusal::EmptyComment => "Write something first.".to_string(),
            Refusal::BadDeadline => "That is not a date.".to_string(),
            Refusal::Cycle => "That link would put this task behind itself.".to_string(),
            Refusal::MovedAlready => {
                "Somebody moved this card first. This is where it is now.".to_string()
            }
            Refusal::NotFound => "No such task.".to_string(),
            Refusal::Unavailable => "Something went wrong. Try again.".to_string(),
        }
    }

    /// The refusal as a short word, for the address bar.
    ///
    /// A browser without script never sees a call's return value: it posts the
    /// form, follows the redirect, and the page it lands on has to be told what
    /// happened. That telling goes through the query, so every refusal needs a
    /// name that survives a round trip through a URL.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Rejected => "rejected",
            Refusal::RateLimited => "rate-limited",
            // The problem itself, not the wording, so the sentence is built
            // here on the way back rather than reflected out of the address.
            Refusal::Password(problem) => {
                if problem == TOO_SHORT {
                    "password-short"
                } else if problem == LOOKS_LIKE_YOU {
                    "password-you"
                } else {
                    "rejected"
                }
            }
            Refusal::Forbidden => "forbidden",
            Refusal::SignInFirst => "sign-in-first",
            Refusal::AlreadyClaimed => "already-claimed",
            Refusal::AddressTaken => "address-taken",
            Refusal::LinkNotUsable => "link-not-usable",
            Refusal::EmptyTitle => "empty-title",
            Refusal::EmptyName => "empty-name",
            Refusal::BadLimit => "bad-limit",
            Refusal::BadFileType => "bad-file-type",
            Refusal::EmptyComment => "empty-comment",
            Refusal::BadDeadline => "bad-deadline",
            Refusal::Cycle => "cycle",
            Refusal::MovedAlready => "moved-already",
            Refusal::NotFound => "not-found",
            Refusal::Unavailable => "unavailable",
        }
    }

    /// The refusal a `code` names, or nothing. Nothing for an unknown word: the
    /// query is whatever the address bar holds, so a code that is not one of
    /// ours says nothing at all rather than something invented.
    pub fn from_code(code: &str) -> Option<Refusal> {
        Some(match code {
            "rejected" => Refusal::Rejected,
            "rate-limited" => Refusal::RateLimited,
            "password-short" => Refusal::Password(TOO_SHORT.to_string()),
            "password-you" => Refusal::Password(LOOKS_LIKE_YOU.to_string()),
            "forbidden" => Refusal::Forbidden,
            "sign-in-first" => Refusal::SignInFirst,
            "already-claimed" => Refusal::AlreadyClaimed,
            "address-taken" => Refusal::AddressTaken,
            "link-not-usable" => Refusal::LinkNotUsable,
            "empty-title" => Refusal::EmptyTitle,
            "empty-comment" => Refusal::EmptyComment,
            "bad-deadline" => Refusal::BadDeadline,
            "cycle" => Refusal::Cycle,
            "moved-already" => Refusal::MovedAlready,
            "not-found" => Refusal::NotFound,
            "unavailable" => Refusal::Unavailable,
            _ => return None,
        })
    }
}

/// Which call a carried refusal belongs to. Two forms on one page must not both
/// claim the same sentence, so the redirect names the call and this is the name:
/// the last piece of the server function's path, with anything that is not a
/// plain word dropped so it can only ever be compared, never rendered.
pub fn call_id(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// What a call refused with, however the answer came back.
///
/// With script the answer is the action's value. Without it there is no value —
/// the browser posted a form and followed a redirect — so the refusal rides
/// back in the address as `?refusal=<code>&on=<call>` and is read from there.
/// The `on` is checked against this call, so the composer does not show the
/// link picker's refusal.
pub fn refusal_of<S>(action: ServerAction<S>) -> impl Fn() -> Option<Refusal> + Copy + Send + Sync
where
    S: leptos::server_fn::ServerFn<Output = Option<Refusal>> + Send + Sync + Clone + 'static,
    S::Error: Clone + Send + Sync + 'static,
{
    let query = leptos_router::hooks::use_query_map();
    move || {
        if let Some(answer) = action.value().get() {
            return match answer {
                Ok(refusal) => refusal,
                // The call never arrived, which is not the person's mistake and
                // is not a sentence about what they asked for.
                Err(_) => Some(Refusal::Unavailable),
            };
        }
        let query = query.read();
        if query.get("on")? != call_id(S::PATH) {
            return None;
        }
        Refusal::from_code(&query.get("refusal")?)
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::*;

    #[test]
    fn every_refusal_survives_the_address_bar() {
        let all = [
            Refusal::Rejected,
            Refusal::RateLimited,
            Refusal::Password(TOO_SHORT.to_string()),
            Refusal::Password(LOOKS_LIKE_YOU.to_string()),
            Refusal::Forbidden,
            Refusal::SignInFirst,
            Refusal::AlreadyClaimed,
            Refusal::AddressTaken,
            Refusal::LinkNotUsable,
            Refusal::EmptyTitle,
            Refusal::EmptyComment,
            Refusal::BadDeadline,
            Refusal::Cycle,
            Refusal::MovedAlready,
            Refusal::NotFound,
            Refusal::Unavailable,
        ];
        for refusal in all {
            assert_eq!(
                Refusal::from_code(refusal.code()).as_ref(),
                Some(&refusal),
                "{} did not come back as itself",
                refusal.code()
            );
        }
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn the_password_wordings_match_the_store() {
        use dizey_core::auth::PasswordProblem;
        assert_eq!(PasswordProblem::TooShort.to_string(), TOO_SHORT);
        assert_eq!(PasswordProblem::LooksLikeYou.to_string(), LOOKS_LIKE_YOU);
    }

    #[test]
    fn an_unknown_code_says_nothing() {
        assert_eq!(Refusal::from_code("not-a-refusal"), None);
        assert_eq!(Refusal::from_code(""), None);
    }

    #[test]
    fn a_call_is_named_by_its_last_path_piece() {
        assert_eq!(call_id("/api/link_tasks"), "link_tasks");
        assert_eq!(call_id("/api/link_tasks?x=1"), "link_tasksx1");
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
pub async fn sign_in(email: String, password: String) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, client_label, set_session_cookie};
    use dizey_core::accounts::SESSION_LIFETIME;

    match accounts().sign_in(&email, &password, &client_label()).await {
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
