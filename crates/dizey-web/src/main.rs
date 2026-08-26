#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() {
    use dizey_core::accounts::Accounts;
    use dizey_core::store::TursoStore;
    use leptos::prelude::*;
    use std::sync::Arc;

    // Every variable the app reads is resolved here, before anything is
    // opened. A missing one stops the boot with its name in the message: the
    // failure this prevents is not an empty database, it is a second Dizey
    // writing a different file while everyone believes they share a board.
    let config = match dizey_core::Config::from_env() {
        Ok(config) => config,
        Err(problem) => {
            eprintln!("dizey: {problem}");
            std::process::exit(2);
        }
    };
    // Said once, so the answer to "which file are we on" lives in the log.
    for line in config.report() {
        println!("dizey    {line}");
    }

    // One process per database file: Turso is a single-writer engine and a
    // second process on the same file loses writes rather than queueing.
    //
    // `open` applies any unapplied migration before it returns.
    let store = TursoStore::open(&config.database.to_string_lossy())
        .await
        .expect("failed to open the database");
    let accounts = Accounts::new(Arc::new(store));

    let conf = get_configuration(None).expect("failed to read leptos configuration");
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;

    let app = dizey_web::server::router(accounts, leptos_options);

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
