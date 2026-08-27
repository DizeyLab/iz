//! The document shell every page renders inside, ported from
//! `izlek-web/src/app.rs`'s `shell`/`App`/`NotFound`.

use topcoat::{
    Result,
    asset::{Asset, asset},
    context::Cx,
    router::{
        StatusCode, page,
        layout,
        error::{NotFoundError, not_found},
    },
    view::view,
};

use crate::i18n::{Key, Lang, t};
use crate::server::current_user;

/// The topbar's signed-in identity: the display name, opening on hover or
/// focus onto details (name, address, role), settings and sign-out. Shared
/// by every signed-in page's topbar (board, settings, mail rules, logs).
pub async fn user_menu(cx: &Cx, display_name: &str, email: &str, role: izlek_core::Role, lang: Lang) -> Result {
    let role_key = match role {
        izlek_core::Role::Admin => Key::RoleAdminOption,
        izlek_core::Role::Member => Key::RoleMemberOption,
        izlek_core::Role::Viewer => Key::RoleViewerOption,
    };
    view! {
        cx =>
        <div class="user-menu">
            <button type="button" class="user-menu-trigger">(display_name.to_string())</button>
            <div class="user-menu-panel">
                <div class="user-menu-name">(display_name.to_string())</div>
                <div class="user-menu-email">(email.to_string())</div>
                <div class="user-menu-role">(t(lang, role_key))</div>
                <div class="user-menu-divider"></div>
                <a class="user-menu-item" href="/settings">(t(lang, Key::Settings))</a>
                <form class="user-menu-item-form" method="post" action="/api/sign_out">
                    <button class="user-menu-item" type="submit">(t(lang, Key::SignOut))</button>
                </form>
            </div>
        </div>
    }
}

/// Every path that matches no page raises a `NotFoundError`, so it renders
/// through `root_layout`'s catch below rather than the router's bare default.
/// `/` itself is served by `landing` above and never reaches this route.
#[page("/{*path}")]
async fn missing() -> Result {
    Err(not_found().into())
}

/// `style/main.scss`, compiled by `build.rs` into `assets/main.css`.
const STYLE: Asset = asset!("assets/main.css");

#[layout("/")]
async fn root_layout(cx: &Cx, slot: Result) -> Result {
    let content = match slot {
        Err(error) if error.downcast_ref::<NotFoundError>().is_some() => view! {
            (StatusCode::NOT_FOUND)
            <main class="scaffold-note">
                <p>"Nothing at this address."</p>
            </main>
        },
        content => content,
    }?;

    // Pages with no session (auth screens) render light and English; both are
    // only set when the request's own user has one to read.
    let asking = match current_user(cx).await {
        Ok(user) => user.as_ref(),
        Err(_) => None,
    };
    let dark = asking.is_some_and(|user| user.theme == "dark");
    let lang = asking.map_or(Lang::En, |user| Lang::from_code(&user.language));

    view! {
        <!DOCTYPE html>
        <html lang=(lang.code()) data-theme=(dark.then_some("dark"))>
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <link rel="preconnect" href="https://fonts.googleapis.com">
                <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin="">
                <link
                    rel="stylesheet"
                    href="https://fonts.googleapis.com/css2?family=Schibsted+Grotesk:wght@400;500;600;700&family=IBM+Plex+Mono:wght@400;500&display=swap"
                >
                <title>"Izlek"</title>
                <link rel="stylesheet" href=(STYLE)>
                topcoat::runtime::script()
                topcoat::dev::script()
            </head>
            <body>
                (content)
            </body>
        </html>
    }
}
