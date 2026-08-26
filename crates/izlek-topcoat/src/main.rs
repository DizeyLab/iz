use izlek_core::accounts::Accounts;
use izlek_core::store::TursoStore;
use std::sync::Arc;
use topcoat::Result;
use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::cookie::RouterBuilderCookieExt;
use topcoat::router::{Router, RouterBuilderDiscoverExt, route};

#[route(GET "/healthz")]
async fn healthz() -> Result<&'static str> {
    Ok("ok")
}

#[tokio::main]
async fn main() {
    // config/izlek.toml is read here, before anything is opened, and written with
    // development defaults if it is not there yet. A broken key stops the
    // boot with its name in the message: the failure this prevents is not an
    // empty database, it is a second Izlek writing a different file while
    // everyone believes they share a board.
    let config = match izlek_core::Config::load() {
        Ok(config) => config,
        Err(problem) => {
            eprintln!("izlek: {problem}");
            std::process::exit(2);
        }
    };
    // Said once, so the answer to "which file are we on" lives in the log.
    for line in config.report() {
        println!("izlek    {line}");
    }

    // One process per database file: Turso is a single-writer engine and a
    // second process on the same file loses writes rather than queueing.
    //
    // `open` applies any unapplied migration before it returns.
    let store = TursoStore::open(&config.database.to_string_lossy())
        .await
        .expect("failed to open the database");
    let store: Arc<dyn izlek_core::store::Store> = Arc::new(store);
    let accounts = Accounts::new(store.clone(), config.base_url.clone());

    // The engine is always built, because a sender can appear at any moment:
    // an admin fills the panel in and the next sweep sends what was held. It
    // holds one connection pool, rebuilt only when the settings behind it
    // change, and two things use it — every committed crossing, and the sweep.
    let engine = Arc::new(izlek_core::MailEngine::new(
        store.clone(),
        Arc::new(izlek_topcoat::smtp::WorkspaceSmtp::new(store.clone())),
        config.base_url.clone(),
    ));
    tokio::spawn(sweep(engine.clone()));

    let router = Router::builder()
        .discover()
        .cookies()
        .assets(AssetBundle::load().expect("failed to load the asset bundle"))
        .app_context(accounts)
        .app_context(engine)
        .build();

    topcoat::start(router).await.expect("server error");
}

/// Retries what a mail server refused earlier and picks up anything a crash
/// left claimed but unsent.
///
/// A minute is short enough that the first retry of a host that blinked lands
/// while somebody is still looking at the board, and long enough that a host
/// which is properly down is not hammered — the wait per send widens on its
/// own, and a send that has used its attempts is written off rather than
/// carried forever.
async fn sweep(engine: std::sync::Arc<izlek_core::MailEngine>) {
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
                "izlek mail  sweep: {} sent, {} to retry, {} given up on",
                report.sent, report.failed, report.abandoned
            ),
            Ok(_) => {}
            Err(problem) => eprintln!("izlek mail  sweep could not read the ledger: {problem}"),
        }
    }
}
