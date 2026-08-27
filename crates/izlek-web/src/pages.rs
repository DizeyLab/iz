//! The account screens, ported from `izlek-web/src/pages.rs`'s FirstLogin
//! artboard: setting up the first account, an invited member picking a
//! password, and signing in.
//!
//! Every page here is server-rendered on every request rather than fetched
//! once and patched: there is no hydration, so the gate a browser sees is
//! decided fresh each time and a refused form post is answered by a redirect
//! whose query the page reads back (`refusal_of`), not by a client-side
//! action value.

use izlek_core::store::User;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{page, path_param};
use topcoat::view::view;

use crate::auth::{Invited, invited_by_token};
use crate::server::{accounts, current_user, refusal_of};

path_param!(token);

/// The front door. Which screen this is depends on the workspace: an empty
/// one offers setup, a claimed one offers a sign-in, a signed-in browser gets
/// the board.
#[page("/")]
async fn landing(cx: &Cx) -> Result {
    match current_user(cx).await {
        Ok(Some(user)) => signed_in_shell(cx, user.clone()).await,
        Ok(None) => {
            let claimed = accounts(cx).store().owner().await?.is_some();
            if claimed { sign_in_card(cx).await } else { setup_card(cx).await }
        }
        Err(_) => view! {
            cx =>
            (topbar(cx).await?)
            <main class="scaffold-note">
                <p>"Something went wrong."</p>
            </main>
        },
    }
}

/// "Pick a password" — the invited member's first sign-in, reached from the
/// emailed link. The address was set by the admin and cannot be edited here.
#[page("/join/{token}")]
async fn join(cx: &Cx) -> Result {
    let token: &str = path_param::<Token>(cx);
    match invited_by_token(cx, token).await? {
        Some(person) => join_card(cx, token, person).await,
        None => view! {
            cx =>
            <main class="auth-stage">
                <div class="auth-column">
                    <div class="auth-card">
                        <div class="auth-title">"This link no longer works"</div>
                        <div class="auth-sub">"Sign-in links last seven days."</div>
                    </div>
                </div>
            </main>
        },
    }
}

async fn topbar(cx: &Cx) -> Result {
    view! {
        cx =>
        <header class="topbar">
            <div class="wordmark">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </div>
        </header>
    }
}

async fn password_rules(cx: &Cx) -> Result {
    view! {
        cx =>
        <ul class="auth-rules">
            <li>"at least 10 characters"</li>
            <li>"not your address or your name"</li>
        </ul>
    }
}

/// "Set up Izlek" — the first account in an empty workspace.
async fn setup_card(cx: &Cx) -> Result {
    let refusal = refusal_of(cx, "claim_workspace");

    view! {
        cx =>
        (topbar(cx).await?)
        <main class="auth-stage">
            <div class="auth-column">
                <span class="auth-kicker">"FIRST ACCOUNT — EMPTY WORKSPACE"</span>
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">"Set up Izlek"</div>
                        <div class="auth-sub">
                            "Nobody has signed in yet, so this account administers the workspace: the mail sender, the limits and the member list are yours."
                        </div>
                    </div>
                    <form method="post" action="/api/claim_workspace">
                        <label class="auth-field">
                            <span class="auth-label">"YOUR NAME"</span>
                            <input
                                class="auth-input"
                                type="text"
                                name="display_name"
                                autocomplete="name"
                                required=""
                            >
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">"EMAIL"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="email"
                                name="email"
                                autocomplete="email"
                                required=""
                            >
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">"PASSWORD"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="new-password"
                                required=""
                            >
                        </label>
                        (password_rules(cx).await?)
                        <button class="auth-submit" type="submit">
                            <span class="auth-submit-text">"Create workspace"</span>
                            <span class="auth-key">"↵"</span>
                        </button>
                    </form>
                    if let Some(refusal) = &refusal {
                        <div class="auth-problem">(refusal.message())</div>
                    }
                    <div class="auth-foot">
                        "Mail rules stay quiet until you connect a sender in Settings. Nothing leaves the machine before that."
                    </div>
                </div>
            </div>
        </main>
    }
}

/// The sign-in form for an account that already has a password. It answers
/// the same whether the address is unknown, has no password yet, or the
/// password is wrong — the difference is not the browser's business.
async fn sign_in_card(cx: &Cx) -> Result {
    let refusal = refusal_of(cx, "sign_in");

    view! {
        cx =>
        (topbar(cx).await?)
        <main class="auth-stage">
            <div class="auth-column">
                <span class="auth-kicker">"SIGN IN"</span>
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">"Sign in to Izlek"</div>
                        <div class="auth-sub">
                            "Accounts are made by the admin. If you were invited, use the link you were sent — it is where you choose your password."
                        </div>
                    </div>
                    <form method="post" action="/api/sign_in">
                        <label class="auth-field">
                            <span class="auth-label">"EMAIL"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="email"
                                name="email"
                                autocomplete="username"
                                required=""
                            >
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">"PASSWORD"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="current-password"
                                required=""
                            >
                        </label>
                        <button class="auth-submit" type="submit">
                            <span class="auth-submit-text">"Sign in"</span>
                            <span class="auth-key">"↵"</span>
                        </button>
                    </form>
                    if let Some(refusal) = &refusal {
                        <div class="auth-problem">(refusal.message())</div>
                    }
                </div>
            </div>
        </main>
    }
}

/// "Pick a password" — the card itself, once the token resolved to a real
/// invitation.
async fn join_card(cx: &Cx, token: &str, person: Invited) -> Result {
    let refusal = refusal_of(cx, "redeem_link");
    // Who made the account, when that is still knowable. The fallback names
    // no one rather than naming the wrong person.
    let made_by = match &person.invited_by {
        Some(admin) => format!("{admin} made you an account."),
        None => "An admin made you an account.".to_string(),
    };

    view! {
        cx =>
        <main class="auth-stage">
            <div class="auth-column">
                <span class="auth-kicker">"INVITED MEMBER — FIRST SIGN-IN"</span>
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">"Pick a password"</div>
                        <div class="auth-sub">(made_by)</div>
                    </div>
                    <div class="auth-field">
                        <span class="auth-label">"SIGNING IN AS"</span>
                        <div class="auth-locked">
                            <span class="auth-locked-value">(person.email)</span>
                            <span class="auth-locked-note">"set by the admin"</span>
                        </div>
                    </div>
                    <form method="post" action="/api/redeem_link">
                        <input type="hidden" name="token" value=(token)>
                        <label class="auth-field">
                            <span class="auth-label">"NEW PASSWORD"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="new-password"
                                required=""
                            >
                        </label>
                        (password_rules(cx).await?)
                        <button class="auth-submit" type="submit">
                            <span class="auth-submit-text">"Set password and sign in"</span>
                        </button>
                    </form>
                    if let Some(refusal) = &refusal {
                        <div class="auth-problem">(refusal.message())</div>
                    }
                    <div class="auth-foot">
                        "Name and photo can wait — you land on the board straight after this. The admin cannot see or set your password."
                    </div>
                </div>
            </div>
        </main>
    }
}

/// The signed-in landing, once a session resolves to a real user: the board.
async fn signed_in_shell(cx: &Cx, user: User) -> Result {
    crate::board::board_page(cx, &user).await
}
