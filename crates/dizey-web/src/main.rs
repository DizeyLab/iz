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
    let store: Arc<dyn dizey_core::store::Store> = Arc::new(store);
    let accounts = Accounts::new(store.clone());

    // A workspace without a sender still works: cards move, rules can be
    // written, nothing goes out. With one, the engine is built once — it holds
    // a connection pool — and two things use it: every committed crossing, and
    // the sweep below.
    let mail = match &config.mail {
        Some(sender) => {
            let smtp = dizey_web::smtp::Smtp::new(sender).unwrap_or_else(|problem| {
                eprintln!("dizey: {problem}");
                std::process::exit(2);
            });
            let engine = Arc::new(dizey_core::MailEngine::new(
                store.clone(),
                Arc::new(smtp),
                config.base_url.clone(),
            ));
            tokio::spawn(sweep(engine.clone()));
            dizey_web::server::Mail::sending(engine)
        }
        None => dizey_web::server::Mail::silent(),
    };

    let conf = get_configuration(None).expect("failed to read leptos configuration");
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;

    let app = dizey_web::server::router(accounts, mail, leptos_options);

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

/// Retries what a mail server refused earlier and picks up anything a crash
/// left claimed but unsent.
///
/// A minute is short enough that the first retry of a host that blinked lands
/// while somebody is still looking at the board, and long enough that a host
/// which is properly down is not hammered — the wait per send widens on its
/// own, and a send that has used its attempts is written off rather than
/// carried forever.
#[cfg(feature = "ssr")]
async fn sweep(engine: std::sync::Arc<dizey_core::MailEngine>) {
    /// Enough that a morning's backlog clears in a few passes, few enough that
    /// one pass cannot sit on the mail server for minutes.
    const PER_PASS: u32 = 50;

    let mut every_minute = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        every_minute.tick().await;
        match engine
            .deliver_owed(time::OffsetDateTime::now_utc(), PER_PASS)
            .await
        {
            Ok(report) if report.sent + report.failed + report.abandoned > 0 => println!(
                "dizey mail  sweep: {} sent, {} to retry, {} given up on",
                report.sent, report.failed, report.abandoned
            ),
            Ok(_) => {}
            Err(problem) => eprintln!("dizey mail  sweep could not read the ledger: {problem}"),
        }
    }
}

#[cfg(not(feature = "ssr"))]
fn main() {
    // The wasm bundle enters through `lib.rs::hydrate`; this target is only
    // built with `ssr`.
}
