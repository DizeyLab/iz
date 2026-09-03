use iz_core::accounts::Accounts;
use iz_core::store::TursoStore;
use std::sync::Arc;
use topcoat::Result;
use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::cookie::RouterBuilderCookieExt;
use topcoat::router::{BodyLimit, Router, RouterBuilderDiscoverExt, route};

#[route(GET "/healthz")]
async fn healthz() -> Result<&'static str> {
    // The deploy asserts this against the commit it pushed, so a stale
    // process still holding the port fails the deploy instead of
    // answering a green health check.
    Ok(concat!("ok ", env!("IZ_BUILD_SHA")))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("reconcile") {
        let mut dry_run = false;
        let mut yes = false;
        for arg in &args[2..] {
            match arg.as_str() {
                "--dry-run" => dry_run = true,
                "--yes" => yes = true,
                _ => {
                    eprintln!("iz reconcile: unknown option {arg}");
                    std::process::exit(2);
                }
            }
        }
        let config = match iz_core::Config::load() {
            Ok(config) => config,
            Err(problem) => {
                eprintln!("iz: {problem}");
                std::process::exit(2);
            }
        };
        if let Err(problem) = iz_core::store::reconcile(
            &config.database.to_string_lossy(),
            Some(config.storage.as_path()),
            iz_core::store::ReconcileOptions {
                dry_run,
                yes,
                auto: false,
            },
        )
        .await
        {
            eprintln!("iz reconcile: {problem}");
            std::process::exit(1);
        }
        return;
    }

    // config/iz.toml is read here, before anything is opened, and written with
    // development defaults if it is not there yet. A broken key stops the
    // boot with its name in the message: the failure this prevents is not an
    // empty database, it is a second İz writing a different file while
    // everyone believes they share a board.
    let config = match iz_core::Config::load() {
        Ok(config) => config,
        Err(problem) => {
            eprintln!("iz: {problem}");
            std::process::exit(2);
        }
    };
    // Said once, so the answer to "which file are we on" lives in the log.
    for line in config.report() {
        println!("iz    {line}");
    }

    // The bundle beside the executable is the only stylesheet this process
    // can serve, and nothing in topcoat binds it to this binary's
    // generation: a bundle left behind by another deploy loads as happily
    // as the right one, and the pages then reference a stylesheet whose
    // bytes are days old — the mixed generation a browser once caught on
    // production. The fingerprint build.rs stamped into this binary is
    // checked against the bundle's bytes, and a foreign bundle refuses the
    // boot rather than serving under it.
    let bundle = AssetBundle::load().unwrap_or_else(|err| {
        eprintln!("iz: the asset bundle beside the executable failed to load: {err}");
        std::process::exit(2);
    });
    let stylesheet = match iz_web::server::stylesheet_guard(&bundle) {
        Ok(line) => line,
        Err(problem) => {
            eprintln!("iz: {problem}");
            std::process::exit(2);
        }
    };
    println!("iz    {stylesheet}");

    // Attachments and profile photos are files beside the database now, not
    // bytes in a table. The tree is made before the store opens, because the
    // reconcile an old database triggers on the way extracts every blob into
    // it.
    ensure_storage_tree(&config.storage);

    // One process per database file: Turso is a single-writer engine and a
    // second process on the same file loses writes rather than queueing.
    //
    // `open` applies any unapplied migration before it returns.
    let store = TursoStore::open(&config.database.to_string_lossy(), &config.storage)
        .await
        .expect("failed to open the database");
    let store: Arc<dyn iz_core::store::Store> = Arc::new(store);
    // The address mail links carry when no admin has set one in Settings.
    let accounts = Accounts::new(store.clone(), config.listen_url());

    // The engine is always built, because a sender can appear at any moment:
    // an admin fills the panel in and the next sweep sends what was held. It
    // holds one connection pool, rebuilt only when the settings behind it
    // change, and two things use it — every committed crossing, and the sweep.
    let engine = Arc::new(iz_core::MailEngine::new(
        store.clone(),
        Arc::new(iz_web::smtp::WorkspaceSmtp::new(store.clone())),
        config.listen_url(),
    ));
    tokio::spawn(sweep(engine.clone(), store.clone()));

    // Told when the process is stopping, so the live streams end instead of
    // being waited out. See `iz_web::live::Shutdown`.
    let (stop, stopping) = tokio::sync::watch::channel(false);

    let router = Router::builder()
        .discover()
        .layer(
            BodyLimit::max(iz_web::settings::WIDEST_ATTACHMENT_MB as usize * 1024 * 1024)
                .at("/files"),
        )
        .layer(
            BodyLimit::max(iz_web::settings::WIDEST_PHOTO_MB as usize * 1024 * 1024)
                .at("/api/profile_photo"),
        )
        .cookies()
        .assets(bundle)
        .app_context(accounts)
        .app_context(iz_web::photo::PhotoStamps::default())
        .app_context(iz_web::live::LiveWindow(std::time::Duration::from_secs(
            config.live_seconds,
        )))
        .app_context(iz_web::live::Shutdown(stopping))
        .app_context(iz_web::server::Mail::sending(engine.clone()))
        .build();

    // `topcoat::start` binds HOST/PORT from the environment; the listen
    // address is a config/iz.toml decision, so the listener is bound
    // explicitly against the same value the boot log just printed.
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .expect("failed to bind the listen address");
    // Not `topcoat::serve`, which installs its own signal handler and gives
    // no way to hear it. The handler is taken over so the live streams learn
    // about the stop before the graceful shutdown starts counting: without
    // that, every open tab holds a stream the shutdown waits its full thirty
    // seconds for, and Ctrl+C appears to hang.
    topcoat::serve_until(listener, router, async move {
        shutdown_signal().await;
        let _ = stop.send(true);
    })
    .await
    .expect("server error");
}

/// Makes the storage tree the store keeps binary files in, if it is not
/// there: `<storage>/attachments` and `<storage>/photos`, private to the user
/// the process runs as. A directory that exists is left exactly as it is; one
/// that cannot be made stops the boot — the failure this prevents is a
/// rebuild extracting blobs into a tree that is not there, and it is better
/// met before anything is opened.
fn ensure_storage_tree(storage: &std::path::Path) {
    let make = |dir: &std::path::Path| {
        if let Err(err) = std::fs::create_dir_all(dir) {
            eprintln!("iz: could not create {}: {err}", dir.display());
            std::process::exit(2);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(err) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            {
                eprintln!("iz: could not restrict {}: {err}", dir.display());
                std::process::exit(2);
            }
        }
    };
    make(storage);
    for name in ["attachments", "photos"] {
        make(&storage.join(name));
    }
}

/// Resolves when the process is asked to stop: Ctrl+C, or `SIGTERM` from a
/// service manager.
async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install the Ctrl+C handler");
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

/// Retries what a mail server refused earlier and picks up anything a crash
/// left claimed but unsent.
///
/// It sleeps until the exact moment the next mail falls due rather than waking
/// on a fixed beat. The beat is what made a retry promised for 16:42:47 leave
/// at 16:43 — the row was due and nothing was awake to notice — and a queue
/// that names a second has to mean the second it names.
///
/// Two other things can wake it. A mail being queued announces itself on the
/// live channel, so an invite goes out as soon as it is asked for instead of
/// waiting out somebody else's timer. And an hour is the longest it will sleep
/// regardless, so a clock jump or a row written by something other than this
/// process is picked up on its own.
async fn sweep(
    engine: std::sync::Arc<iz_core::MailEngine>,
    store: Arc<dyn iz_core::store::Store>,
) {
    /// Enough that a morning's backlog clears in a few passes, few enough that
    /// one pass cannot sit on the mail server for minutes.
    const PER_PASS: u32 = 50;
    /// The longest this will sleep with nothing due.
    const IDLE: std::time::Duration = std::time::Duration::from_secs(3600);

    let mut queued = store.subscribe();
    loop {
        match engine
            .deliver_owed(time::OffsetDateTime::now_utc(), PER_PASS)
            .await
        {
            Ok(report) if report.sent + report.failed + report.abandoned > 0 => println!(
                "iz mail  sweep: {} sent, {} to retry, {} given up on",
                report.sent, report.failed, report.abandoned
            ),
            Ok(_) => {}
            Err(problem) => eprintln!("iz mail  sweep could not read the ledger: {problem}"),
        }

        // How long until the next mail is owed. A row already due means the
        // pass above could not take it — it was over its limit, or something
        // else holds it — so the wait is the idle one rather than zero, which
        // would spin.
        let wait = match store.next_due_at().await {
            Ok(Some(at)) => {
                let now = time::OffsetDateTime::now_utc();
                if at > now {
                    (at - now).try_into().unwrap_or(IDLE).min(IDLE)
                } else {
                    IDLE
                }
            }
            Ok(None) => IDLE,
            Err(problem) => {
                eprintln!("iz mail  sweep could not read the ledger: {problem}");
                IDLE
            }
        };

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            // A newly queued mail is due now, so there is no reason to make it
            // wait out a timer that was set before it existed.
            alive = queue_touched(&mut queued) => {
                if !alive {
                    return;
                }
            }
        }
    }
}

/// Waits for something to happen to the mail queue specifically.
///
/// The channel carries every topic, and the sweep cares about one. Filtering
/// here rather than in the `select!` matters: a `continue` on somebody else's
/// board edit would send this round the loop and run a whole delivery pass, so
/// every card moved on the board would poke the mail server. Returns false
/// when the channel is gone, which means the process is going with it.
///
/// Lagging means announcements were dropped — a reason to look, not to stop.
async fn queue_touched(rx: &mut tokio::sync::broadcast::Receiver<iz_core::Change>) -> bool {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(change) => {
                if change.topic == iz_core::Topic::Queue {
                    return true;
                }
            }
            Err(RecvError::Lagged(_)) => return true,
            Err(RecvError::Closed) => return false,
        }
    }
}
