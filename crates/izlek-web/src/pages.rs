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
use crate::i18n::{Key, Lang, made_you_an_account, t};
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
            if claimed {
                sign_in_card(cx).await
            } else {
                setup_card(cx).await
            }
        }
        Err(_) => view! {
            cx =>
            (topbar(cx).await?)
            <main class="scaffold-note">
                <p>(t(Lang::En, Key::SomethingWentWrong))</p>
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
                        <div class="auth-title">(t(Lang::En, Key::LinkExpiredTitle))</div>
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
            (crate::layout::wordmark(cx).await?)
        </header>
    }
}

/// "Set up İzlek" — the first account in an empty workspace.
async fn setup_card(cx: &Cx) -> Result {
    let refusal = refusal_of(cx, "claim_workspace");
    let lang = Lang::En;

    view! {
        cx =>
        (topbar(cx).await?)
        <main class="auth-stage">
            <div class="auth-column">
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">(t(lang, Key::SetupTitle))</div>
                        <div class="auth-sub">(t(lang, Key::SetupSub))</div>
                    </div>
                    <form method="post" action="/api/claim_workspace" data-hard="">
                        <label class="auth-field">
                            <span class="auth-label">(t(lang, Key::YourNameLabel))</span>
                            <input
                                class="auth-input"
                                type="text"
                                name="display_name"
                                autocomplete="name"
                                required=""
                            >
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">(t(lang, Key::EmailLabel))</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="email"
                                name="email"
                                autocomplete="email"
                                required=""
                            >
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">(t(lang, Key::PasswordLabel))</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="new-password"
                                minlength=(izlek_core::auth::MIN_PASSWORD_CHARS.to_string())
                                required=""
                            >
                        </label>
                        <button class="auth-submit" type="submit">
                            <span class="auth-submit-text">(t(lang, Key::CreateWorkspace))</span>
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

/// The sign-in form for an account that already has a password. It answers
/// the same whether the address is unknown, has no password yet, or the
/// password is wrong — the difference is not the browser's business.
async fn sign_in_card(cx: &Cx) -> Result {
    let refusal = refusal_of(cx, "sign_in");
    let lang = Lang::En;

    view! {
        cx =>
        (topbar(cx).await?)
        <main class="auth-stage">
            <div class="auth-column">
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">(t(lang, Key::SignInTitle))</div>
                        <div class="auth-sub">(t(lang, Key::SignInSub))</div>
                    </div>
                    <form method="post" action="/api/sign_in" data-hard="">
                        <label class="auth-field">
                            <span class="auth-label">(t(lang, Key::EmailLabel))</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="email"
                                name="email"
                                autocomplete="username"
                                required=""
                            >
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">(t(lang, Key::PasswordLabel))</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="current-password"
                                required=""
                            >
                        </label>
                        <button class="auth-submit" type="submit">
                            <span class="auth-submit-text">(t(lang, Key::SignInButton))</span>
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
    let lang = Lang::En;
    // Who made the account, when that is still knowable. The fallback names
    // no one rather than naming the wrong person.
    let made_by = match &person.invited_by {
        Some(admin) => made_you_an_account(lang, admin),
        None => t(lang, Key::AdminMadeYouAnAccount).to_string(),
    };

    view! {
        cx =>
        <main class="auth-stage">
            <div class="auth-column">
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">(t(lang, Key::PickPasswordTitle))</div>
                        <div class="auth-sub">(made_by)</div>
                    </div>
                    <div class="auth-field">
                        <span class="auth-label">(t(lang, Key::SigningInAsLabel))</span>
                        <div class="auth-locked">
                            <span class="auth-locked-value">(person.email)</span>
                        </div>
                    </div>
                    <form method="post" action="/api/redeem_link" data-hard="">
                        <input type="hidden" name="token" value=(token)>
                        <label class="auth-field">
                            <span class="auth-label">(t(lang, Key::NewPasswordLabel))</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="new-password"
                                minlength=(izlek_core::auth::MIN_PASSWORD_CHARS.to_string())
                                required=""
                            >
                        </label>
                        <button class="auth-submit" type="submit">
                            <span class="auth-submit-text">(t(lang, Key::SetPasswordAndSignIn))</span>
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

/// The signed-in landing, once a session resolves to a real user: the board.
async fn signed_in_shell(cx: &Cx, user: User) -> Result {
    crate::board::board_page(cx, &user).await
}
