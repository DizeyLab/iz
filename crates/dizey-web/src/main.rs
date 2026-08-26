#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() {
    use axum::Router;
    use dizey_core::accounts::Accounts;
    use dizey_core::store::TursoStore;
    use dizey_web::app::{App, shell};
    use leptos::prelude::*;
    use leptos_axum::{LeptosRoutes, generate_route_list};
    use std::sync::Arc;

    // One process per database file: Turso is a single-writer engine and a
    // second process on the same file loses writes rather than queueing.
    let database = std::env::var("DIZEY_DATABASE").unwrap_or_else(|_| "dizey.db".to_string());
    // `open` applies any unapplied migration before it returns.
    let store = TursoStore::open(&database)
        .await
        .expect("failed to open the database");
    let accounts = Accounts::new(Arc::new(store));

    let conf = get_configuration(None).expect("failed to read leptos configuration");
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;
    let routes = generate_route_list(App);

    let app = Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            {
                // Provided here *and* to the server-function handler, which
                // `leptos_routes_with_context` registers with the same closure.
                let accounts = accounts.clone();
                move || provide_context(accounts.clone())
            },
            {
                let leptos_options = leptos_options.clone();
                move || shell(leptos_options.clone())
            },
        )
        .fallback(leptos_axum::file_and_error_handler(shell))
        .with_state(leptos_options);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");
    println!("dizey listening on http://{addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
        .await
        .expect("server error");
}

#[cfg(not(feature = "ssr"))]
fn main() {
    // The wasm bundle enters through `lib.rs::hydrate`; this target is only
    // built with `ssr`.
}
