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
    // empty database, it is a second iz writing a different file while
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

    // Attachments are files beside the database now, not bytes in a table.
    // The tree is made before the store opens, because the reconcile an old
    // database triggers on the way extracts every blob into it.
    ensure_storage_tree(&config.storage);

    // One process per database file: Turso is a single-writer engine and a
    // second process on the same file loses writes rather than queueing.
    //
    // `open` applies any unapplied migration before it returns.
    let store = TursoStore::open(&config.database.to_string_lossy(), &config.storage)
        .await
        .expect("failed to open the database");
    let store: Arc<dyn iz_core::store::Store> = Arc::new(store);
    // The key sealing the OIDC session cookies, kept beside the database as
    // `iz.key` — one key per deployment, never in the repository.
    let key_path = config
        .database
        .parent()
        .map(|parent| parent.join("iz.key"))
        .unwrap_or_else(|| std::path::PathBuf::from("iz.key"));
    let cookie_key = match iz_core::store::secret::load_or_create_key(&key_path) {
        Ok(key) => key,
        Err(problem) => {
            eprintln!("iz: could not load {}: {problem}", key_path.display());
            std::process::exit(2);
        }
    };
    let oidc = iz_client::Config {
        issuer: config.oidc.issuer.clone(),
        client_id: config.oidc.client_id.clone(),
        client_secret: config.oidc.client_secret.clone(),
        redirect_uri: config.oidc.redirect_uri.clone(),
        // Fallback for where im's /logout sends the browser when a
        // sign-out that started here finishes: this app's configured
        // public address. The stored one answers first — the
        // `LogoutBack` context below is asked before this is.
        logout_back: config.public_url(),
        cookie_name: "iz_session".to_string(),
        cookie_key,
    };
    // The shared identity directory: the same issuer and Basic pair the OIDC
    // side holds, pointed at im's roster. One HTTP pool inside, cheap to
    // clone — the avatar route reaches it through the router context below,
    // and the two mirror tasks carry their own handles. The health beside
    // it is what the Settings Connection card renders: the mirror writes,
    // the page reads.
    let health = iz_web::directory::DirectoryHealth::new();
    let directory = im_client::directory::DirectoryClient::new(
        config.oidc.issuer.clone(),
        config.oidc.client_id.clone(),
        config.oidc.client_secret.clone(),
    );
    // The family's Files service as a storage peer, beside the identity
    // peer above: the client carries the key `[storage.in]` named (and
    // when the deployment never configured one, the client still exists —
    // every call then refuses before the wire), and the health beside it
    // is what the Settings Storage card renders. The beat below keeps
    // both current.
    let storage_client = iz_web::storage::StorageClient::new(
        config.storage_in.as_ref().map(|storage| storage.token.clone()),
    );
    let storage_health = iz_web::storage::StorageHealth::new();
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

    // The member list mirrors im's directory two ways. The live stream is
    // the fast path — a photo uploaded, a rename, a disable in im lands
    // within a moment and announces, so every open board swaps the face
    // without a reload. The beat stays as the watchdog: one full pass
    // every five minutes heals whatever a dead stream hid, and keeps the
    // family list fresh, which the stream does not carry. First passes
    // run right away, so a fresh deploy sees everyone at boot.
    // The address the app files itself under is `base_url` alone: a bound
    // address is no fallback here — a loopback bind is nothing anyone else
    // can reach, and filing it would overwrite the real one in every
    // switcher.
    tokio::spawn(directory_sync(
        store.clone(),
        directory.clone(),
        iz_client::IzClient::new(oidc.clone()),
        config.base_url.clone(),
        health.clone(),
    ));
    tokio::spawn(directory_stream(store.clone(), directory.clone(), health.clone()));
    // The storage mirror's watchdog: follows the family to wherever in
    // lives now, keeps the card's facts current, and — while the
    // workspace's attachments live on in — drains the rows still on this
    // disk. First pass runs right away, so a fresh enablement starts
    // moving rows within the beat.
    tokio::spawn(storage_beat(
        store.clone(),
        storage_client.clone(),
        storage_health.clone(),
    ));
    // Told when the process is stopping, so the live streams end instead of
    // being waited out. See `iz_web::live::Shutdown`.
    let (stop, stopping) = tokio::sync::watch::channel(false);

    let router = iz_client::mount(
        Router::builder()
            .discover()
            .layer(
                BodyLimit::max(iz_web::settings::WIDEST_ATTACHMENT_MB as usize * 1024 * 1024)
                    .at("/files"),
            )
            .cookies()
            .assets(bundle),
        oidc,
    )
    .app_context(store.clone())
    .app_context(directory)
    .app_context(health.clone())
    .app_context(storage_client.clone())
    .app_context(storage_health.clone())
    .app_context(iz_client::LogoutBack(Arc::new(iz_web::server::logout_back)))
    .app_context(config.clone())
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
/// there: `<storage>/attachments`, private to the user the process runs as.
/// A directory that exists is left exactly as it is; one
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
    for name in ["attachments"] {
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
/// waiting out somebody else's timer. And an hour is the longest it will
/// sleep regardless, so a clock jump or a row written by something other
/// than this process is picked up on its own; a row that is due but that the
/// last pass could not take gets a one-minute re-check instead, for the
/// same reason — see the wait computed below.
async fn sweep(engine: std::sync::Arc<iz_core::MailEngine>, store: Arc<dyn iz_core::store::Store>) {
    /// Enough that a morning's backlog clears in a few passes, few enough that
    /// one pass cannot sit on the mail server for minutes.
    const PER_PASS: u32 = 50;
    /// The longest this will sleep with nothing due.
    const IDLE: std::time::Duration = std::time::Duration::from_secs(3600);
    /// How long it re-reads when a row is already due but the pass above
    /// could not take it.
    const RECHECK: std::time::Duration = std::time::Duration::from_secs(60);

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

        // How long until the next mail is owed. A row that is already due
        // but that the pass above could not take gets a short re-check, not
        // the idle hour. A row can sit due for reasons the pass cannot fix
        // on the spot: the pass hit its limit and this row is next in line,
        // or it fell due inside the breath between reading the ledger and
        // claiming. None of that asks the pass to hurry — the claim UPDATE
        // is the arbiter, and a row another pass is composing has already
        // had its next_attempt_at pushed into the future, so re-reading
        // cannot send it twice — but none of it justifies an hour either:
        // the hour is what once turned a one-second blind spot in the stamp
        // compare into a reminder twenty-five minutes late. It is not zero,
        // which would spin: the pass above has only just run.
        let wait = match store.next_due_at().await {
            Ok(Some(at)) => {
                let now = time::OffsetDateTime::now_utc();
                if at > now {
                    (at - now).try_into().unwrap_or(IDLE).min(IDLE)
                } else {
                    RECHECK
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

/// How often the member list is re-mirrored from im. Short enough that a
/// person invited to im shows up here before anyone goes looking for them,
/// long enough that the two services are not talking about it constantly.
const DIRECTORY_SECONDS: u64 = 300;

/// Keeps the member list a mirror of im's directory, and the switcher's
/// family list a mirror of im's admin panel, on the beat. The live stream
/// (see [`directory_stream`]) is the fast path; this loop is the watchdog —
/// every pass is a full read of a short list, so whatever a dead stream
/// quietly missed is healed within five minutes. An im that does not
/// answer — down, restarting, mid-deploy — costs one log line and nothing
/// else: the rows stay, and the next beat asks again.
async fn directory_sync(
    store: Arc<dyn iz_core::store::Store>,
    directory: im_client::directory::DirectoryClient,
    client: iz_client::IzClient,
    configured_url: String,
    health: iz_web::directory::DirectoryHealth,
) {
    // An address that never arrives would otherwise say so every beat,
    // five minutes apart, forever. It is one deployment fact, so it is
    // said once and the mirror goes on without it.
    let mut said_no_address = false;
    loop {
        mirror_directory(&store, &directory, &health).await;
        // Before asking for the family, this app files itself in it, so a
        // deployment appears in everyone's switcher without an admin
        // typing its address. Resolved per beat in the order a sign-out
        // resolves it — the stored public address an admin set wins over
        // the configured one — because both can change while the process
        // runs. Registration is announcement, not permission: a refusal,
        // an im too old to know the route, a dropped connection all cost
        // one line and the fetch below still runs.
        let origin = match store.get_setting(iz_web::server::PUBLIC_URL_KEY).await {
            Ok(Some(stored)) if !stored.trim().is_empty() => stored.trim().to_string(),
            _ => configured_url.trim().to_string(),
        };
        let origin = origin.trim_end_matches('/').to_string();
        if origin.is_empty() {
            if !said_no_address {
                said_no_address = true;
                eprintln!(
                    "family: no public address to register; set base_url or the Server rail's public address"
                );
            }
        } else if let Err(problem) = client.register_family("iz", "Board", &origin).await {
            eprintln!("family register: {problem}");
        }
        // The family rides the same beat: one JSON array in one setting
        // row, the same shape im's `/family` served this pass. `family`
        // refuses a body it cannot read whole rather than storing half a
        // switcher, so what lands in the row is always renderable.
        match client.family().await {
            Some(family) => match serde_json::to_string(&family) {
                Ok(json) => {
                    if let Err(problem) =
                        store.set_setting(iz_web::server::FAMILY_KEY, &json).await
                    {
                        eprintln!("family sync: {problem}");
                    }
                }
                Err(problem) => eprintln!("family sync: {problem}"),
            },
            None => eprintln!("family sync: im did not answer; keeping the list there is"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(DIRECTORY_SECONDS)).await;
    }
}

/// One full pass of im's roster through the member rows: every entry
/// through [`iz_core::store::Store::sync_member`], which announces only
/// the rows that actually changed. A pass that landed also stamps the
/// Connection card's "last full pass".
async fn mirror_directory(
    store: &Arc<dyn iz_core::store::Store>,
    directory: &im_client::directory::DirectoryClient,
    health: &iz_web::directory::DirectoryHealth,
) {
    match directory.directory().await {
        Ok(members) => {
            for member in members {
                apply_member(store, &member).await;
            }
            health.pass();
        }
        Err(problem) => {
            eprintln!("directory sync: im did not answer ({problem}); keeping the rows there are")
        }
    }
}

async fn apply_member(
    store: &Arc<dyn iz_core::store::Store>,
    member: &im_client::directory::DirectoryMember,
) {
    if let Err(problem) = store
        .sync_member(
            &member.sub,
            &member.email,
            &member.name,
            member.admin,
            member.photo_version,
            &member.timezone,
        )
        .await
    {
        eprintln!("directory sync: {}: {problem}", member.email);
    }
}

/// The live half of the mirror: im's `/directory/live`, one event per
/// changed member. Forever, in this shape: a full pass, then the stream;
/// whenever the stream ends — im restarting, or its fifty-minute window
/// closing — wait out a backoff that doubles from one second to thirty,
/// replay a full pass (the resync that heals whatever the dead stream
/// carried or dropped), and redial. A stream error inside the body is the
/// same exit as a clean close: the pass below is what converges. The
/// Connection card reads its state from the same marks this loop leaves:
/// Connected from the open stream and each event, Reconnecting from every
/// exit in between.
async fn directory_stream(
    store: Arc<dyn iz_core::store::Store>,
    directory: im_client::directory::DirectoryClient,
    health: iz_web::directory::DirectoryHealth,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        mirror_directory(&store, &directory, &health).await;
        match directory.open_stream().await {
            Ok(mut stream) => {
                health.connected();
                backoff = std::time::Duration::from_secs(1);
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(im_client::directory::DirectoryEvent::Profile(member)) => {
                            health.event();
                            apply_member(&store, &member).await;
                        }
                        Err(problem) => {
                            eprintln!("directory stream: {problem}; re-listing");
                            health.reconnecting();
                            break;
                        }
                    }
                }
                health.reconnecting();
            }
            Err(problem) => {
                eprintln!("directory stream: im did not answer ({problem})");
                health.reconnecting();
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
    }
}

/// The storage beat: [`iz_web::storage::storage_cycle`] over and over,
/// [`iz_web::storage::STORAGE_SECONDS`] apart. The cycle body is extracted
/// the way `mirror_directory` is, so a test can run one beat by hand; this
/// loop is only the alarm clock.
async fn storage_beat(
    store: Arc<dyn iz_core::store::Store>,
    client: iz_web::storage::StorageClient,
    health: iz_web::storage::StorageHealth,
) {
    loop {
        iz_web::storage::storage_cycle(&store, &client, &health).await;
        tokio::time::sleep(std::time::Duration::from_secs(
            iz_web::storage::STORAGE_SECONDS,
        ))
        .await;
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
    loop {
        match rx.recv().await {
            Ok(change) => {
                if change.topic == iz_core::Topic::Queue {
                    return true;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return true,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
        }
    }
}
