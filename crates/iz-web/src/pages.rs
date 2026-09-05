//! The front door.
//!
//! Which screen `/` is depends on the browser: a signed-in one is sent on
//! to the board, a signed-out one is offered the sign-in card. There is no
//! setup screen — accounts are provisioned by signing in through im, and the
//! first im admin ever seen claims the workspace owner.
//!
//! Every page here is server-rendered on every request rather than fetched
//! once and patched: there is no hydration, so the gate a browser sees is
//! decided fresh each time.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::page;
use topcoat::view::view;

use crate::i18n::{Key, Lang, t};
use crate::layout::wordmark;
use crate::server::current_user;

/// The front door: the board for a signed-in browser, the sign-in card for
/// everybody else.
#[page("/")]
async fn landing(cx: &Cx) -> Result {
    match current_user(cx).await {
        Ok(Some(user)) => crate::board::board_page(cx, &user).await,
        Ok(None) => sign_in_card(cx).await,
        Err(_) => view! {
            cx =>
            <main class="scaffold-note">
                <p>(t(Lang::En, Key::SomethingWentWrong))</p>
            </main>
        },
    }
}

/// The sign-in card for a browser with nobody in it: the wordmark and one
/// link, which starts the OIDC round-trip at `/auth/login`. A round-trip
/// that just failed names nothing but the fact.
async fn sign_in_card(cx: &Cx) -> Result {
    let language = Lang::En;
    let failed = topcoat::router::request::uri(cx)
        .query()
        .is_some_and(|query| {
            query
                .split('&')
                .any(|pair| pair.starts_with("auth_error="))
        });
    view! {
        cx =>
        <main class="auth-stage">
            <div class="auth-column">
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">(wordmark(cx).await?)</div>
                        <div class="auth-sub">(t(language, Key::WelcomeBlurb))</div>
                    </div>
                    <a class="auth-submit" href="/auth/login">
                        <span class="auth-submit-text">(t(language, Key::SignIn))</span>
                    </a>
                    if failed {
                        <div class="auth-problem">(t(language, Key::SignInFailed))</div>
                    }
                </div>
            </div>
        </main>
    }
}
