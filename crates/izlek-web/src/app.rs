use leptos::prelude::*;
use leptos_meta::{MetaTags, Stylesheet, Title, provide_meta_context};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::{SsrMode, path};

use crate::pages::{Join, Landing};
use crate::rules::RulesPage;
use crate::settings::SettingsPage;

/// The HTML document the server streams. `cargo-leptos` writes the bundle to
/// `/pkg/izlek.{js,wasm,css}`, which is what the tags below point at.
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <link rel="preconnect" href="https://fonts.googleapis.com"/>
                <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin=""/>
                <link
                    rel="stylesheet"
                    href="https://fonts.googleapis.com/css2?family=Schibsted+Grotesk:wght@400;500;600;700&family=IBM+Plex+Mono:wght@400;500&display=swap"
                />
                <AutoReload options=options.clone()/>
                <HydrationScripts options/>
                <MetaTags/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    view! {
        <Stylesheet id="leptos" href="/pkg/izlek.css"/>
        <Title text="Izlek"/>
        <Router>
            // In-order streaming, not the out-of-order default. Out-of-order ships
            // the page inside a <template> and relies on a script to move it into
            // place, so a browser without JavaScript gets an empty <main>. In-order
            // pauses the stream at each <Suspense> and writes the real markup where
            // it belongs, which is what makes the forms below reachable at all.
            <Routes fallback=NotFound>
                <Route path=path!("/") view=Landing ssr=SsrMode::InOrder/>
                <Route path=path!("/join/:token") view=Join ssr=SsrMode::InOrder/>
                <Route path=path!("/rules") view=RulesPage ssr=SsrMode::InOrder/>
                <Route path=path!("/settings") view=SettingsPage ssr=SsrMode::InOrder/>
            </Routes>
        </Router>
    }
}

#[component]
fn NotFound() -> impl IntoView {
    #[cfg(feature = "ssr")]
    {
        let resp = expect_context::<leptos_axum::ResponseOptions>();
        resp.set_status(axum::http::StatusCode::NOT_FOUND);
    }

    view! {
        <main class="scaffold-note">
            <p>"Nothing at this address."</p>
        </main>
    }
}
