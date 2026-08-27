//! The document shell every page renders inside, ported from
//! `izlek-web/src/app.rs`'s `shell`/`App`/`NotFound`.

use topcoat::{
    Result,
    asset::{Asset, asset},
    router::{
        StatusCode, page,
        layout,
        error::{NotFoundError, not_found},
    },
    view::view,
};

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
async fn root_layout(slot: Result) -> Result {
    let content = match slot {
        Err(error) if error.downcast_ref::<NotFoundError>().is_some() => view! {
            (StatusCode::NOT_FOUND)
            <main class="scaffold-note">
                <p>"Nothing at this address."</p>
            </main>
        },
        content => content,
    }?;

    view! {
        <!DOCTYPE html>
        <html lang="en">
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
