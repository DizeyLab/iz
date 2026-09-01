//! The app driven the way a browser drives it: real router, real handlers, real
//! session cookie.
//!
//! The point of this binary is the guards. A button the UI does not draw proves
//! nothing — a Viewer who posts to the mutation endpoint anyway must be refused
//! by the handler, and that is what these tests call.
//!
//! New HTTP tests belong in this file rather than a new `tests/*.rs`: one test
//! binary links and runs once.
//!
//! Ported from the Leptos-era `izlek-web`'s `tests/http.rs`. That app answered
//! a hydrated caller with `200` and a JSON body; this one is server-rendered
//! and every mutating `/api/*` route always answers `303 See Other` (topcoat's
//! `see_other`, not the old stack's `302 Found`), carrying the same JSON body
//! (`Option<Refusal>` or a domain value) that this file's assertions already
//! read off `Answer::body`. `path::<ServerFn>()` is gone with the server
//! functions themselves — routes are now literal `/api/...` strings, kept as
//! `const` items below so a rename shows up once.
#![cfg(test)]

use std::path::PathBuf;
use std::sync::Arc;

use http::{HeaderValue, Request, StatusCode, header};
use izlek_core::Role;
use izlek_core::accounts::Accounts;
use izlek_core::board::Moved;
use izlek_core::store::{Audience, MailOutcome, SendKind, SendState, Store, Trigger, TursoStore};
use izlek_web::server::{Mail, SESSION_COOKIE};
use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::cookie::RouterBuilderCookieExt;
use topcoat::router::{Body, BodyLimit, Router, RouterBuilderDiscoverExt, to_bytes};
use ulid::Ulid;

/// The bundle `cargo build -p izlek-web` + `topcoat asset bundle --bin
/// izlek-web` write next to the crate's own `target/debug`, not next to the
/// test binary (which lives in `target/debug/deps`) — `AssetBundle::load`
/// looks beside `current_exe` and would miss it, so the path is given
/// explicitly instead.
fn asset_dir() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/debug/assets"
    ))
}

/// A live connection in the suite outlives any test that reads it. The bound
/// on a live test is [`live_until`]'s own timeout — waiting for the frame it
/// expects — not this window: a window short enough to end the stream promptly
/// is also short enough to lose the race on a loaded machine, which is a
/// flaky test rather than a fast one.
const TEST_LIVE_WINDOW: izlek_web::live::LiveWindow =
    izlek_web::live::LiveWindow(std::time::Duration::from_secs(10));

/// A throwaway workspace: its own database file and its own router.
struct App {
    dir: PathBuf,
    router: Router,
    store: Arc<dyn Store>,
    /// Stands in for Ctrl+C. Held so a test can stop this router the way the
    /// process stops the real one.
    stop: tokio::sync::watch::Sender<bool>,
}

impl App {
    async fn build(base_url: &str, mail: Mail) -> Self {
        let dir = std::env::temp_dir().join(format!("izlek-http-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store: Arc<dyn Store> = Arc::new(
            TursoStore::open(dir.join("izlek.db").to_str().unwrap())
                .await
                .unwrap(),
        );
        let accounts = Accounts::new(store.clone(), base_url);
        let (stop, _) = tokio::sync::watch::channel(false);
        let router = Router::builder()
            .discover()
            .layer(
                BodyLimit::max(izlek_web::settings::WIDEST_ATTACHMENT_MB as usize * 1024 * 1024)
                    .at("/files"),
            )
            .cookies()
            .assets(
                AssetBundle::load_dir(asset_dir())
                    .expect("run `topcoat asset bundle` before the http suite"),
            )
            .app_context(accounts)
            .app_context(izlek_web::photo::PhotoStamps::default())
            .app_context(TEST_LIVE_WINDOW)
            .app_context(izlek_web::live::Shutdown(stop.subscribe()))
            .app_context(mail)
            .build();
        Self {
            dir,
            router,
            store,
            stop,
        }
    }

    async fn open() -> Self {
        Self::build("http://127.0.0.1:3000", Mail::silent()).await
    }

    /// Like `open`, but with a live mail engine reading the workspace's SMTP
    /// settings, so a transition actually reaches the ledger instead of the
    /// silent no-op `open`'s router hands every crossing.
    async fn open_with_mail() -> Self {
        let dir = std::env::temp_dir().join(format!("izlek-http-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store: Arc<dyn Store> = Arc::new(
            TursoStore::open(dir.join("izlek.db").to_str().unwrap())
                .await
                .unwrap(),
        );
        let engine = Arc::new(izlek_core::MailEngine::new(
            store.clone(),
            Arc::new(izlek_web::smtp::WorkspaceSmtp::new(store.clone())),
            "https://izlek.sh",
        ));
        let accounts = Accounts::new(store.clone(), "https://izlek.sh");
        let (stop, _) = tokio::sync::watch::channel(false);
        let router = Router::builder()
            .discover()
            .layer(
                BodyLimit::max(izlek_web::settings::WIDEST_ATTACHMENT_MB as usize * 1024 * 1024)
                    .at("/files"),
            )
            .cookies()
            .assets(
                AssetBundle::load_dir(asset_dir())
                    .expect("run `topcoat asset bundle` before the http suite"),
            )
            .app_context(accounts)
            .app_context(izlek_web::photo::PhotoStamps::default())
            .app_context(TEST_LIVE_WINDOW)
            .app_context(izlek_web::live::Shutdown(stop.subscribe()))
            .app_context(Mail::sending(engine))
            .build();
        Self {
            dir,
            router,
            store,
            stop,
        }
    }

    /// Opens `/api/live`. The response is returned as soon as the handler has
    /// subscribed, so a caller writes AFTER this returns and the announcement
    /// still lands in the feed.
    async fn live_open(&self, cookie: Option<&str>) -> topcoat::router::response::Response<Body> {
        let mut request = Request::builder().method("GET").uri("/api/live");
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        self.router
            .handle(request.body(Body::empty()).unwrap())
            .await
    }

    /// The single workspace's id, read straight off the store: `TursoStore` is
    /// single-tenant, so there is no id to guess and no JSON endpoint needed
    /// just to hand it back.
    async fn workspace_id(&self) -> String {
        self.store
            .workspace()
            .await
            .unwrap()
            .expect("no workspace yet")
            .id
    }

    /// Posts a form the way a hydrated caller does: `Accept: application/json`,
    /// no `Referer`. Every mutating `/api/*` route answers `303 See Other`
    /// regardless — [`Router::handle`] never follows a redirect, so the answer
    /// is read straight off this response, same as `oneshot` did before.
    async fn post(&self, path: &str, cookie: Option<&str>, form: &[(&str, &str)]) -> Answer {
        self.post_with_extra_cookie(path, cookie, form, None).await
    }

    /// Like `post`, but with a second `Cookie` header of the caller's own
    /// naming — for the `izlek_rows_<section>` page-size cookie, which lives
    /// beside the session cookie rather than replacing it.
    async fn post_with_extra_cookie(
        &self,
        path: &str,
        cookie: Option<&str>,
        form: &[(&str, &str)],
        extra_cookie: Option<&str>,
    ) -> Answer {
        let body = form
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        if let Some(extra) = extra_cookie {
            request = request.header(header::COOKIE, HeaderValue::from_str(extra).unwrap());
        }
        let response = self
            .router
            .handle(request.body(Body::from(body)).unwrap())
            .await;
        Answer::from_response(response).await
    }

    /// Posts the same form the way a browser with no script posts it: asking
    /// for a page back, from a page it names. The answer is a redirect, and the
    /// redirect is the whole of what the person will be shown.
    async fn post_without_script(
        &self,
        path: &str,
        cookie: Option<&str>,
        referer: &str,
        form: &[(&str, &str)],
    ) -> Answer {
        let body = form
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::ACCEPT, "text/html")
            .header(header::REFERER, referer)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        let response = self
            .router
            .handle(request.body(Body::from(body)).unwrap())
            .await;
        let mut answer = Answer::from_response(response).await;
        answer.session = None;
        answer
    }

    /// Posts a multipart form the way a browser's
    /// `<form enctype="multipart/form-data">` does, hand-built rather than
    /// pulling in a client crate for it: the fields first, in the order given
    /// — the upload handler reads `task_id` before it reaches `file` — then
    /// one `file` part if given.
    async fn post_multipart(
        &self,
        path: &str,
        cookie: Option<&str>,
        fields: &[(&str, &str)],
        file: Option<(&str, &str, &[u8])>,
    ) -> Answer {
        const BOUNDARY: &str = "izlek-test-boundary";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        if let Some((filename, content_type, bytes)) = file {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let mut request = Request::builder().method("POST").uri(path).header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        let response = self
            .router
            .handle(request.body(Body::from(body)).unwrap())
            .await;
        let mut answer = Answer::from_response(response).await;
        answer.session = None;
        answer
    }

    /// Gets a page or a download the way a browser does: no `Accept` header
    /// forcing JSON, an optional cookie, and the raw bytes back untouched — a
    /// download's body is not always UTF-8.
    async fn get(&self, path: &str, cookie: Option<&str>) -> Raw {
        self.get_with_range(path, cookie, None).await
    }

    /// Like `get`, but with a second `Cookie` header of the caller's own
    /// naming — for the `izlek_rows_<section>` page-size cookie, which
    /// lives beside the session cookie rather than replacing it.
    async fn get_with_extra_cookie(&self, path: &str, cookie: Option<&str>, extra: &str) -> Raw {
        self.get_with(path, cookie, None, None, Some(extra)).await
    }

    /// Like `get`, but with a `Range` header, for exercising the
    /// `/files/{id}` partial-content path.
    async fn get_with_range(&self, path: &str, cookie: Option<&str>, range: Option<&str>) -> Raw {
        self.get_with(path, cookie, range, None, None).await
    }

    /// Like `get`, but with `If-None-Match`, for the `/photo/{user_id}`
    /// revalidate path.
    async fn get_with_if_none_match(&self, path: &str, cookie: Option<&str>, etag: &str) -> Raw {
        self.get_with(path, cookie, None, Some(etag), None).await
    }

    async fn get_with(
        &self,
        path: &str,
        cookie: Option<&str>,
        range: Option<&str>,
        if_none_match: Option<&str>,
        extra_cookie: Option<&str>,
    ) -> Raw {
        let mut request = Request::builder().method("GET").uri(path);
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        if let Some(extra) = extra_cookie {
            request = request.header(header::COOKIE, HeaderValue::from_str(extra).unwrap());
        }
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        if let Some(if_none_match) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, if_none_match);
        }
        let response = self
            .router
            .handle(request.body(Body::empty()).unwrap())
            .await;
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let content_range = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let accept_ranges = response
            .headers()
            .get(header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Raw {
            status,
            content_type,
            disposition,
            content_range,
            accept_ranges,
            etag,
            bytes,
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A GET answer kept as raw bytes: a page's HTML or a download's file, neither
/// of which the JSON-shaped [`Answer`] below is meant for.
struct Raw {
    status: StatusCode,
    content_type: Option<String>,
    disposition: Option<String>,
    content_range: Option<String>,
    accept_ranges: Option<String>,
    etag: Option<String>,
    bytes: Vec<u8>,
}

struct Answer {
    status: StatusCode,
    session: Option<String>,
    body: String,
    /// Where a browser without script is sent next. The only place such a
    /// browser can be told anything.
    location: Option<String>,
}

impl Answer {
    async fn from_response(response: topcoat::router::response::Response) -> Self {
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let session = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(session_from);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Answer {
            status,
            session,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            location,
        }
    }
}

/// The `Set-Cookie` value's session token, if that header carries a live one.
fn session_from(raw: &str) -> Option<String> {
    let value = raw.strip_prefix(SESSION_COOKIE)?.strip_prefix('=')?;
    let value = value.split(';').next()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Form encoding, enough for the addresses and passphrases these tests send.
fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// What the tests type into the password field. It must never come back out.
const SENDER_PASSWORD: &str = "cavalry-battery-hinge-40";

/// Fills in the sender panel the way an admin would, and reports whether the
/// server accepted it.
///
/// `save_sender` answers with an empty body and carries any refusal on the
/// redirect's query instead of in JSON (settings routes have no hydrated
/// caller to answer with a value) — accepted is "no `refusal=` on the way
/// back".
async fn sender_saved(app: &App, admin: &str) -> bool {
    let answer = app
        .post(
            "/api/save_sender",
            Some(admin),
            &[
                ("host", "smtp.fastmail.com"),
                ("port", "465"),
                ("username", "izlek"),
                ("password", SENDER_PASSWORD),
                ("from_name", "İzlek"),
                ("from_address", "izlek@izlek.sh"),
            ],
        )
        .await;
    answer.status == StatusCode::SEE_OTHER
        && answer
            .location
            .as_deref()
            .is_some_and(|location| !location.contains("refusal="))
}

/// Claims the workspace and returns the admin's session cookie.
async fn admin(app: &App) -> String {
    let answer = app
        .post(
            "/api/claim_workspace",
            None,
            &[
                ("display_name", "Ada Lovelace"),
                ("email", "ada@izlek.sh"),
                ("password", "correct horse battery staple"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "null", "claiming was refused");
    answer.session.expect("claiming set no session cookie")
}

/// The join token no longer rides the server function's answer — only the
/// invitee's address does. It rides the invite mail this function digs out of
/// the outbox instead: the newest pending invite queued for that address.
async fn queued_join_token(app: &App, email: &str) -> String {
    let sends = app
        .store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let body = sends
        .into_iter()
        .rev()
        .find(|send| {
            send.kind == SendKind::Invite && send.rule_id.is_none() && send.recipient == email
        })
        .and_then(|send| send.body)
        .expect("no invite mail queued for {email}");
    body.rsplit_once("/join/")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .expect("no invitation link in the mail body")
        .to_string()
}

/// Invites someone in the given role and signs them in, returning their cookie.
#[tokio::test]
async fn an_admin_can_send_a_signin_link_to_somebody_who_already_has_a_password() {
    // Somebody who forgets their password is the person who needs a fresh
    // link, and they were the one person the button was hidden from: it was
    // drawn only while `!has_password`. There was no other way back into the
    // workspace — no reset anywhere — so a forgotten password was the end of
    // that account.
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    // `invited` redeems the link, so this member has a password.
    let _member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let page = app
        .get("/settings?section=members", Some(&admin_cookie))
        .await;
    let page = String::from_utf8_lossy(&page.bytes);
    assert!(
        page.contains("Send a sign-in link"),
        "no way to send a link to a member who has a password: {page}"
    );

    // And the link it sends actually works: a new password, set on the
    // strength of it.
    let workspace_id = app.workspace_id().await;
    let member_id = app
        .store
        .user_by_email(&workspace_id, "emre@izlek.sh")
        .await
        .unwrap()
        .expect("the member is not in the store")
        .id;
    let answer = app
        .post(
            "/api/resend_link",
            Some(&admin_cookie),
            &[("user_id", &member_id)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);

    let token = queued_join_token(&app, "emre@izlek.sh").await;
    let answer = app
        .post(
            "/api/redeem_link",
            None,
            &[
                ("token", &token),
                ("password", "second thoughts about oats"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "null", "the reissued link was refused");

    let back = app
        .post(
            "/api/sign_in",
            None,
            &[
                ("email", "emre@izlek.sh"),
                ("password", "second thoughts about oats"),
            ],
        )
        .await;
    assert!(
        back.session.is_some(),
        "the new password does not sign in: {}",
        back.body
    );
}

#[tokio::test]
async fn a_password_may_be_made_of_punctuation() {
    // An invited member reported "it would not accept the *". Nothing in the
    // rules mentions characters, so if one is refused it is the wire eating
    // it, not the policy: a form body is url-encoded, where `+` means space
    // and `%` starts an escape, and a password that survives the round trip
    // in a test is a password that survives it in a browser.
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let email = "punct@izlek.sh";
    let answer = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", email),
                ("display_name", "Pat"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let token = queued_join_token(&app, email).await;

    let password = "*+&%= ?#/\\ tulip 42";
    let answer = app
        .post(
            "/api/redeem_link",
            None,
            &[("token", &token), ("password", password)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "null", "the punctuation password was refused");

    // And it is the same password on the way back in — an encoding that ate a
    // character on the way in would let them set one thing and sign in with
    // another.
    let back = app
        .post(
            "/api/sign_in",
            None,
            &[("email", email), ("password", password)],
        )
        .await;
    assert_eq!(back.status, StatusCode::SEE_OTHER, "{}", back.body);
    assert!(
        back.session.is_some(),
        "the password that was set does not sign in: {}",
        back.body
    );
}

async fn invited(app: &App, admin: &str, email: &str, name: &str, role: Role) -> String {
    let role = match role {
        Role::Admin => "admin",
        Role::Member => "member",
        Role::Viewer => "viewer",
    };
    let answer = app
        .post(
            "/api/invite_member",
            Some(admin),
            &[("email", email), ("display_name", name), ("role", role)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let token = queued_join_token(app, email).await;

    let answer = app
        .post(
            "/api/redeem_link",
            None,
            &[
                ("token", &token),
                ("password", "lantern gravel spoon meadow"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "null", "first sign-in was refused");
    answer.session.expect("first sign-in set no session cookie")
}

/// The id of the board's first column, read straight off the store: there is
/// no JSON `CurrentBoard` endpoint left to ask for it (the board is a
/// server-rendered shard now), and the id is fixture setup, not the behavior
/// under test.
async fn first_column(app: &App) -> String {
    columns_of(app)
        .await
        .into_iter()
        .next()
        .expect("no columns on a fresh board")
}

/// Every column id on the board, in order, read straight off the store — see
/// [`first_column`] on why this bypasses HTTP.
async fn columns_of(app: &App) -> Vec<String> {
    let workspace_id = app.workspace_id().await;
    let board = app
        .store
        .board(&workspace_id)
        .await
        .unwrap()
        .expect("no board");
    app.store
        .columns(&board.id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect()
}

/// Makes a task and hands back its id, read straight off the store — there is
/// no `CurrentBoard` JSON call left to read it from (the board is a
/// server-rendered shard now); the mutation itself is still a real HTTP post.
async fn a_task(app: &App, cookie: &str, column: &str, title: &str) -> String {
    let answer = app
        .post(
            "/api/create_task",
            Some(cookie),
            &[("title", title), ("column_id", column)],
        )
        .await;
    assert_eq!(answer.body, "", "the task was refused: {}", answer.body);

    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    board
        .columns
        .iter()
        .flat_map(|c| &c.cards)
        .find(|card| card.title == title)
        .expect("the new task is not on the board")
        .id
        .clone()
}

#[tokio::test]
async fn adding_a_member_mails_them_the_link() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", "nour@izlek.sh"),
                ("display_name", "Nour"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("nour@izlek.sh"), "{}", answer.body);
    assert!(!answer.body.contains("/join/"), "{}", answer.body);

    let sends = app
        .store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let invites: Vec<_> = sends
        .iter()
        .filter(|send| {
            send.kind == SendKind::Invite
                && send.rule_id.is_none()
                && send.recipient == "nour@izlek.sh"
        })
        .collect();
    assert_eq!(invites.len(), 1, "{invites:?}");
    assert!(
        invites[0]
            .body
            .as_deref()
            .unwrap_or_default()
            .contains("/join/"),
        "{:?}",
        invites[0]
    );
}

/// A resend queues a second invite mail rather than replacing the first —
/// the outbox keeps both attempts.
#[tokio::test]
async fn a_resend_queues_another_mail() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let invitation = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", "sena@izlek.sh"),
                ("display_name", "Sena"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(invitation.status, StatusCode::OK, "{}", invitation.body);

    let workspace_id = app.workspace_id().await;
    let sena_id = app
        .store
        .users(&workspace_id)
        .await
        .unwrap()
        .into_iter()
        .find(|user| user.email == "sena@izlek.sh")
        .expect("no member row for sena")
        .id;

    let count = |sends: &[izlek_core::store::MailSend]| {
        sends
            .iter()
            .filter(|send| {
                send.kind == SendKind::Invite
                    && send.rule_id.is_none()
                    && send.recipient == "sena@izlek.sh"
            })
            .count()
    };
    let before = count(
        &app.store
            .mail_queue(10, izlek_core::store::FeedPage::Newest)
            .await
            .unwrap(),
    );
    assert_eq!(before, 1);

    // `resend_link` has no hydrated action to answer with a value here: the
    // mailed address rides the redirect's query instead of a JSON body.
    let resent = app
        .post(
            "/api/resend_link",
            Some(&admin_cookie),
            &[("user_id", &sena_id)],
        )
        .await;
    assert_eq!(resent.status, StatusCode::SEE_OTHER, "{}", resent.body);
    let location = resent.location.expect("resend did not redirect");
    assert!(location.contains("mailed=sena%40izlek.sh"), "{location}");

    let after = count(
        &app.store
            .mail_queue(10, izlek_core::store::FeedPage::Newest)
            .await
            .unwrap(),
    );
    assert_eq!(after, 2);
}

#[tokio::test]
async fn a_viewer_who_posts_to_create_task_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "quiet@izlek.sh", "Quiet Reader", Role::Viewer).await;
    let column = first_column(&app).await;

    let answer = app
        .post(
            "/api/create_task",
            Some(&viewer),
            &[
                ("title", "Viewer should not get this"),
                ("column_id", &column),
            ],
        )
        .await;

    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "");
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=create_task"),
        "{:?}",
        answer.location
    );

    // And the refusal is not cosmetic: the board is still empty.
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .all(|card| card.title != "Viewer should not get this"),
        "the refused task was written anyway"
    );
}

#[tokio::test]
async fn a_member_may_create_a_task() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@izlek.sh", "Mo Dubois", Role::Member).await;
    let column = first_column(&app).await;

    let answer = app
        .post(
            "/api/create_task",
            Some(&member),
            &[("title", "Wire the deadline chip"), ("column_id", &column)],
        )
        .await;
    assert_eq!(answer.body, "");
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "a create that worked said it was refused: {:?}",
        answer.location
    );

    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    let card = board
        .columns
        .iter()
        .flat_map(|c| &c.cards)
        .find(|card| card.title == "Wire the deadline chip")
        .expect("new task is not on the board");
    let tail = card
        .task_key
        .strip_prefix("DZ-")
        .expect("key not shaped like DZ-<tail>");
    assert!(
        (5..=7).contains(&tail.len()),
        "key tail {tail} not 5..=7 chars"
    );
    assert!(
        tail.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
        "key tail {tail} not uppercase alnum"
    );
}

// A cross-site form post carries no refusal body — the create simply works —
// so `carry_refusal_on_redirect` used to have nothing to rewrite and sent the
// browser straight to the attacker's own Referer. The fix sanitizes the
// Location on every redirect this layer sees, refusal or not.
#[tokio::test]
async fn a_successful_create_never_redirects_off_site() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app).await;

    let answer = app
        .post_without_script(
            "/api/create_task",
            Some(&admin),
            "https://elsewhere.example/steal",
            &[("title", "Cross-site create"), ("column_id", &column)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let location = answer.location.expect("a redirect with nowhere to go");
    assert!(
        !location.contains("elsewhere.example"),
        "the browser was sent off-site: {location}"
    );
}

#[tokio::test]
async fn a_task_cannot_be_dropped_into_another_workspaces_column() {
    let app = App::open().await;
    let admin = admin(&app).await;

    let answer = app
        .post(
            "/api/create_task",
            Some(&admin),
            &[
                ("title", "Wrong column"),
                ("column_id", "00000000-0000-0000-0000-000000000000"),
            ],
        )
        .await;
    assert_eq!(answer.body, "");
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=create_task"),
        "{:?}",
        answer.location
    );
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .all(|card| card.title != "Wrong column"),
        "the refused task was written anyway"
    );
}

#[tokio::test]
async fn a_card_needs_a_title() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app).await;

    let answer = app
        .post(
            "/api/create_task",
            Some(&admin),
            &[("title", "   "), ("column_id", &column)],
        )
        .await;
    assert_eq!(answer.body, "");
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=empty-title&on=create_task"),
        "{:?}",
        answer.location
    );
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        board.columns.iter().flat_map(|c| &c.cards).count(),
        0,
        "a blank title was stored anyway"
    );
}

#[tokio::test]
async fn the_board_is_not_readable_without_a_session() {
    let app = App::open().await;
    let _admin = admin(&app).await;

    // There is no `CurrentBoard` JSON call any more (see FINDING near
    // `first_column`); the guard is exercised on a real read instead: the
    // signed-out board page itself.
    let raw = app.get("/", None).await;
    let html = String::from_utf8_lossy(&raw.bytes);
    assert!(
        !html.contains("board-stage"),
        "a signed-out browser was shown the board: {html}"
    );
}

/// The sort control still posts as a plain `<select name=sort>` — `dropdown.rs`
/// only hides it and stands a trigger button in front, it never renames or
/// drops the form field a `GET /?sort=` relies on.
#[tokio::test]
async fn the_board_sort_control_keeps_its_hidden_select_and_gets_a_dropdown_trigger() {
    let app = App::open().await;
    let admin = admin(&app).await;

    let page = app.get("/", Some(&admin)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains("select class=\"status-select\" name=\"sort\""),
        "{html}"
    );
    // `dropdown.rs`'s script is what turns that hidden select into the
    // house trigger + panel client-side; its presence is the page's only
    // server-rendered proof the shell is wired in.
    assert!(
        html.contains("dd-trigger"),
        "no dropdown script on the board page: {html}"
    );
    // `root_layout` emits `layout.rs`'s Escape manager on every page; the
    // registration below is proof the topbar `.user-menu` gets its close
    // path on the board page too.
    assert!(
        html.contains("window.__izlekEsc"),
        "no escape manager on the board page: {html}"
    );
    assert!(
        html.contains("__izlekEsc.register(40"),
        "no escape script on the board page: {html}"
    );
    assert!(
        html.contains("closest('.user-menu')"),
        "no user-menu close path on the board page: {html}"
    );
}

#[tokio::test]
async fn a_viewer_who_posts_a_comment_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "eyes@izlek.sh", "Ida Eyes", Role::Viewer).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin, &column, "Ship the detail modal").await;

    let answer = app
        .post(
            "/api/post_comment",
            Some(&viewer),
            &[("task_id", &task), ("body", "Viewers cannot say this")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "\"Forbidden\"");

    // The refusal is not cosmetic: nothing was written.
    let answer = app
        .post("/api/fetch_task", Some(&admin), &[("task_id", &task)])
        .await;
    assert!(
        !answer.body.contains("Viewers cannot say this"),
        "the refused comment was written anyway: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_member_may_comment_and_the_author_is_the_session() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "kai@izlek.sh", "Kai Renner", Role::Member).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin, &column, "Wire the picker").await;

    let answer = app
        .post(
            "/api/post_comment",
            Some(&member),
            &[("task_id", &task), ("body", "Picker is narrow on purpose")],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    let answer = app
        .post("/api/fetch_task", Some(&admin), &[("task_id", &task)])
        .await;
    assert!(answer.body.contains("Picker is narrow on purpose"));
    assert!(
        answer.body.contains("Kai Renner"),
        "the comment is not attributed to the session's user: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_link_that_would_close_a_circle_is_refused_at_the_endpoint() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    let answer = app
        .post(
            "/api/link_tasks",
            Some(&admin),
            &[
                ("task_id", &second),
                ("other_id", &first),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(
        answer.body, "null",
        "the first link was refused: {}",
        answer.body
    );

    let answer = app
        .post(
            "/api/link_tasks",
            Some(&admin),
            &[
                ("task_id", &first),
                ("other_id", &second),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(answer.body, "\"Cycle\"");
}

#[tokio::test]
async fn a_refusal_reaches_a_browser_with_no_script() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    app.post(
        "/api/link_tasks",
        Some(&admin),
        &[
            ("task_id", &second),
            ("other_id", &first),
            ("direction", "blocked_by"),
        ],
    )
    .await;

    // The same link back the other way, asked for by a browser that has no way
    // to read the answer's body.
    let answer = app
        .post_without_script(
            "/api/link_tasks",
            Some(&admin),
            &format!("http://izlek.test/?task={first}"),
            &[
                ("task_id", &first),
                ("other_id", &second),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let location = answer.location.expect("a redirect with nowhere to go");
    assert!(
        location.contains("refusal=cycle") && location.contains("on=link_tasks"),
        "the refusal did not ride back on the redirect: {location}"
    );
    assert!(
        location.contains(&format!("task={first}")),
        "the modal was closed on the way back: {location}"
    );
}

#[tokio::test]
async fn a_call_that_was_not_refused_carries_nothing_back() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    let answer = app
        .post_without_script(
            "/api/link_tasks",
            Some(&admin),
            "http://izlek.test/",
            &[
                ("task_id", &second),
                ("other_id", &first),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let location = answer.location.expect("a redirect with nowhere to go");
    assert!(
        !location.contains("refusal="),
        "a link that was made said it was refused: {location}"
    );
}

#[tokio::test]
async fn a_task_id_from_nowhere_is_not_found() {
    let app = App::open().await;
    let admin = admin(&app).await;

    let answer = app
        .post(
            "/api/fetch_task",
            Some(&admin),
            &[("task_id", "00000000-0000-0000-0000-000000000000")],
        )
        .await;
    assert_eq!(answer.body, "{\"Err\":\"NotFound\"}");
}

#[tokio::test]
async fn a_viewer_who_posts_a_delete_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "wren@izlek.sh", "Wren Ash", Role::Viewer).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin, &column, "Viewers cannot remove this").await;

    let answer = app
        .post("/api/delete_task", Some(&viewer), &[("task_id", &task)])
        .await;
    assert_eq!(answer.body, "\"Forbidden\"");

    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .any(|card| card.title == "Viewers cannot remove this"),
        "the refused delete happened anyway"
    );
}

#[tokio::test]
async fn deleting_the_task_the_modal_came_from_lands_on_the_board_not_the_dead_modal() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin, &column, "Delete me from my own modal").await;

    let answer = app
        .post_without_script(
            "/api/delete_task",
            Some(&admin),
            &format!("http://izlek.test/?task={task}"),
            &[("task_id", &task)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/"),
        "a deleted task's modal should not reopen"
    );
}

#[tokio::test]
async fn a_member_may_delete_and_the_delete_is_soft() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "rae@izlek.sh", "Rae Okonkwo", Role::Member).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin, &column, "Mistyped in a hurry").await;

    // What it would cost is a read: it says so and writes nothing.
    let answer = app
        .post(
            "/api/what_delete_costs",
            Some(&member),
            &[("task_id", &task)],
        )
        .await;
    assert!(
        answer.body.contains("Mistyped in a hurry"),
        "{}",
        answer.body
    );
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .any(|card| card.title == "Mistyped in a hurry"),
        "asking cost deleted it"
    );

    let answer = app
        .post("/api/delete_task", Some(&member), &[("task_id", &task)])
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    // Gone from the board, and gone from the detail: soft is not visible.
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .all(|card| card.title != "Mistyped in a hurry"),
        "the task is still on the board"
    );
    let answer = app
        .post("/api/fetch_task", Some(&admin), &[("task_id", &task)])
        .await;
    assert_eq!(answer.body, "{\"Err\":\"NotFound\"}");
}

#[tokio::test]
async fn a_viewer_who_posts_a_move_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "quiet@izlek.sh", "Quiet Reader", Role::Viewer).await;
    let columns = columns_of(&app).await;
    let task = a_task(&app, &admin, &columns[0], "Stays in Backlog").await;

    let answer = app
        .post(
            "/api/move_card",
            Some(&viewer),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;

    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "");
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=move_card"),
        "{:?}",
        answer.location
    );

    // And nothing moved: the refusal is in the handler, not in the drawing.
    let answer = app
        .post("/api/fetch_task", Some(&admin), &[("task_id", &task)])
        .await;
    assert!(
        answer.body.contains(&columns[0]),
        "the refused move happened anyway: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_member_may_move_a_card() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@izlek.sh", "Mo Dubois", Role::Member).await;
    let columns = columns_of(&app).await;
    let task = a_task(&app, &admin, &columns[0], "Gets picked up").await;

    let answer = app
        .post(
            "/api/move_card",
            Some(&member),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(answer.body, "");
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "a move that worked said it was refused: {:?}",
        answer.location
    );

    let answer = app
        .post("/api/fetch_task", Some(&admin), &[("task_id", &task)])
        .await;
    assert!(answer.body.contains("\"moved\""), "no move in the activity");
}

// Same open-redirect shape as create_task's: an empty (successful) body left
// the Referer's host untouched before the fix.
#[tokio::test]
async fn a_successful_move_never_redirects_off_site() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let task = a_task(&app, &admin, &columns[0], "Cross-site move").await;

    let answer = app
        .post_without_script(
            "/api/move_card",
            Some(&admin),
            "https://elsewhere.example/steal",
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let location = answer.location.expect("a redirect with nowhere to go");
    assert!(
        !location.contains("elsewhere.example"),
        "the browser was sent off-site: {location}"
    );
}

#[tokio::test]
async fn a_card_with_open_subtasks_is_refused_the_done_column() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let parent = a_task(&app, &admin, &columns[0], "Ship the exporter").await;
    let child = a_task(&app, &admin, &columns[0], "Write the CSV writer").await;
    app.store.set_parent(&child, Some(&parent)).await.unwrap();

    // The last column is the done one; both the board drag and the detail
    // page's status control post here.
    let done = columns.last().unwrap();
    let held = app
        .post(
            "/api/move_card",
            Some(&admin),
            &[
                ("task_id", &parent),
                ("from_column_id", &columns[0]),
                ("to_column_id", done),
            ],
        )
        .await;
    assert!(
        held.location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=subtasks-open&on=move_card"),
        "{:?}",
        held.location
    );

    // The card did not move, so the board still shows it where it was.
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    let held_card = board
        .columns
        .iter()
        .flat_map(|c| &c.cards)
        .find(|card| card.id == parent)
        .expect("the parent left the board");
    assert_eq!(held_card.column_id, columns[0]);
    assert!(!held_card.is_done());

    // Finishing the subtask is what lets it through.
    let finished = app
        .post(
            "/api/move_card",
            Some(&admin),
            &[
                ("task_id", &child),
                ("from_column_id", &columns[0]),
                ("to_column_id", done),
            ],
        )
        .await;
    assert_eq!(finished.body, "");
    let through = app
        .post(
            "/api/move_card",
            Some(&admin),
            &[
                ("task_id", &parent),
                ("from_column_id", &columns[0]),
                ("to_column_id", done),
            ],
        )
        .await;
    assert_eq!(through.body, "");
    assert!(
        !through
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        through.location
    );
}

#[tokio::test]
async fn a_subtask_is_opened_from_its_parents_page_and_shows_up_there() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let parent = a_task(&app, &admin, &columns[0], "Ship the exporter").await;

    let made = app
        .post(
            "/api/create_subtask",
            Some(&admin),
            &[("parent_id", &parent), ("title", "Write the CSV writer")],
        )
        .await;
    assert_eq!(made.body, "null", "the subtask was refused: {}", made.body);

    // The parts live on their own tab; the board does not card them.
    let page = app
        .get(&format!("/?task={parent}&tab=subtasks"), Some(&admin))
        .await;
    let page = String::from_utf8_lossy(&page.bytes);
    assert!(
        page.contains("Write the CSV writer"),
        "the subtask is not on its parent's Subtasks tab"
    );

    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    let cards: Vec<_> = board.columns.iter().flat_map(|c| &c.cards).collect();
    assert_eq!(cards.len(), 1, "the subtask took a card of its own");
    assert_eq!(cards[0].subtask_label().as_deref(), Some("0/1"));
}

#[tokio::test]
async fn a_subtask_with_nothing_in_its_title_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let parent = a_task(&app, &admin, &columns[0], "Ship the exporter").await;

    // Posted the way a browser without script does, so the refusal has to
    // survive the round trip through the address bar.
    let empty = app
        .post_without_script(
            "/api/create_subtask",
            Some(&admin),
            &format!("http://izlek.sh/?task={parent}"),
            &[("parent_id", &parent), ("title", "   ")],
        )
        .await;
    assert!(
        empty
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=empty-title&on=create_subtask"),
        "{:?}",
        empty.location
    );
}

#[tokio::test]
async fn taking_a_task_in_and_letting_it_out_are_the_same_form() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let parent = a_task(&app, &admin, &columns[0], "Ship the exporter").await;
    let loose = a_task(&app, &admin, &columns[1], "Write the CSV writer").await;

    let taken = app
        .post(
            "/api/set_parent",
            Some(&admin),
            &[("task_id", &loose), ("parent_id", &parent)],
        )
        .await;
    assert_eq!(
        taken.body, "null",
        "taking it in was refused: {}",
        taken.body
    );
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(board.columns.iter().flat_map(|c| &c.cards).count(), 1);

    // The subtask kept its column: being taken in is not a move.
    let page = app.get(&format!("/?task={loose}"), Some(&admin)).await;
    let page = String::from_utf8_lossy(&page.bytes);
    // The way back up: a link that names the whole, not a caption.
    assert!(
        page.contains("detail-crumb-up") && page.contains(&format!("/?task={parent}")),
        "the part does not link back to its whole"
    );
    assert!(
        page.contains("Ship the exporter"),
        "the link back does not name the task it goes to"
    );

    let released = app
        .post(
            "/api/set_parent",
            Some(&admin),
            &[("task_id", &loose), ("parent_id", "")],
        )
        .await;
    assert_eq!(
        released.body, "null",
        "letting it out was refused: {}",
        released.body
    );
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        board.columns.iter().flat_map(|c| &c.cards).count(),
        2,
        "the released task did not come back to the board"
    );
}

#[tokio::test]
async fn a_second_level_of_subtask_is_refused_at_the_door() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let parent = a_task(&app, &admin, &columns[0], "Ship the exporter").await;
    let child = a_task(&app, &admin, &columns[0], "Write the CSV writer").await;
    let loose = a_task(&app, &admin, &columns[0], "Loose").await;
    app.store.set_parent(&child, Some(&parent)).await.unwrap();

    let refused = app
        .post_without_script(
            "/api/create_subtask",
            Some(&admin),
            &format!("http://izlek.sh/?task={child}"),
            &[("parent_id", &child), ("title", "Deeper")],
        )
        .await;
    assert!(
        refused
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=not-nestable&on=create_subtask"),
        "{:?}",
        refused.location
    );

    let also_refused = app
        .post_without_script(
            "/api/set_parent",
            Some(&admin),
            &format!("http://izlek.sh/?task={child}"),
            &[("task_id", &loose), ("parent_id", &child)],
        )
        .await;
    assert!(
        also_refused
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=not-nestable&on=set_parent"),
        "{:?}",
        also_refused.location
    );
}

#[tokio::test]
async fn a_viewer_may_not_open_or_move_a_subtask() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let watcher = invited(&app, &admin, "kay@izlek.sh", "Kay Watcher", Role::Viewer).await;
    let columns = columns_of(&app).await;
    let parent = a_task(&app, &admin, &columns[0], "Ship the exporter").await;

    let refused = app
        .post_without_script(
            "/api/create_subtask",
            Some(&watcher),
            &format!("http://izlek.sh/?task={parent}"),
            &[("parent_id", &parent), ("title", "Not yours")],
        )
        .await;
    assert!(
        refused
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden"),
        "{:?}",
        refused.location
    );

    let also = app
        .post_without_script(
            "/api/set_parent",
            Some(&watcher),
            &format!("http://izlek.sh/?task={parent}"),
            &[("task_id", &parent), ("parent_id", "")],
        )
        .await;
    assert!(
        also.location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden"),
        "{:?}",
        also.location
    );
}

#[tokio::test]
async fn a_card_wears_its_conditional_classes() {
    // `class:foo=(cond)` renders as a literal attribute in this view macro
    // rather than merging into `class`, so every state that used it — a done
    // card, an overdue chip, a dateless chip, an open-parts chip — was
    // invisible. They are built with `class!` now; this holds that.
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let task = a_task(&app, &admin, &columns[0], "Undated").await;

    let page = app.get("/", Some(&admin)).await;
    let page = String::from_utf8_lossy(&page.bytes);
    assert!(
        !page.contains("class:"),
        "a conditional class rendered as a literal attribute"
    );
    assert!(
        page.contains("card-deadline card-deadline-none"),
        "the dateless chip lost its class"
    );

    // A card in a done column carries card-done, which is what greys it.
    let done = columns.last().unwrap();
    app.post(
        "/api/move_card",
        Some(&admin),
        &[
            ("task_id", &task),
            ("from_column_id", &columns[0]),
            ("to_column_id", done),
        ],
    )
    .await;
    let page = app.get("/", Some(&admin)).await;
    let page = String::from_utf8_lossy(&page.bytes);
    assert!(
        page.contains("card card-done"),
        "a finished card is not marked"
    );
}

#[tokio::test]
async fn a_parents_own_part_is_not_offered_as_a_blocker() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let parent = a_task(&app, &admin, &columns[0], "Ship the exporter").await;
    let child = a_task(&app, &admin, &columns[0], "Write the CSV writer").await;
    let stranger = a_task(&app, &admin, &columns[0], "Unrelated work").await;
    app.store.set_parent(&child, Some(&parent)).await.unwrap();

    // The store refuses a parent-to-part edge, so the picker must not offer
    // one: an option that is always refused is a worse control than no option.
    let page = app.get(&format!("/?task={parent}"), Some(&admin)).await;
    let page = String::from_utf8_lossy(&page.bytes);
    let picker = page
        .split_once("link-pop")
        .map(|(_, rest)| rest.split_once("</div>").map(|(p, _)| p).unwrap_or(rest))
        .unwrap_or_default()
        .to_string();
    assert!(
        !picker.contains(&child),
        "the picker offered the task's own part"
    );
    assert!(
        page.contains(&stranger),
        "an unrelated task vanished from the page"
    );
}

#[tokio::test]
async fn a_file_input_without_a_name_label_still_submits() {
    // The shared change handler used to read `.file-upload-name` out of the
    // input's label and set its text. The profile photo's label holds an
    // avatar and no such span, so the read threw and the submit that came
    // after it never ran — the photo silently never uploaded. The handler
    // tolerates a missing name now; this holds both halves of that.
    let app = App::open().await;
    let admin = admin(&app).await;
    let page = app.get("/settings", Some(&admin)).await;
    let page = String::from_utf8_lossy(&page.bytes);

    assert!(
        page.contains("avatar-upload"),
        "the profile photo control is gone"
    );
    assert!(
        page.contains("var name = label ? label.querySelector('.file-upload-name') : null"),
        "the file-input handler assumes a name label again"
    );
    assert!(
        page.contains("if (name && control.files && control.files[0])"),
        "the name write is unguarded again"
    );
}

#[tokio::test]
async fn the_whole_control_box_opens_its_dropdown() {
    // A status control is a box holding a dot, the trigger and a chevron.
    // Listening on the trigger alone leaves every other part of the box dead
    // where it looks clickable. The delegated listener walks up to whatever
    // *directly* contains a single trigger, which pins the hit area to the
    // visual box and needs no markup to opt in.
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let task = a_task(&app, &admin, &columns[0], "Anything").await;
    let page = app.get(&format!("/?task={task}"), Some(&admin)).await;
    let page = String::from_utf8_lossy(&page.bytes);

    assert!(
        page.contains("found[0].parentNode === box"),
        "the dropdown hit area is back to the button alone"
    );
    assert!(
        page.contains("trigger.__ddPanel = panel"),
        "a delegated click can no longer reach the panel"
    );
    // The box really is a box with siblings around the select — which is the
    // shape that made the button-only listener wrong in the first place.
    assert!(
        page.contains("status-dot") && page.contains("status-select"),
        "the status control changed shape"
    );
}

#[tokio::test]
async fn the_morph_keeps_what_the_client_owns_and_names_none_of_it() {
    // The live refresh morphs the server's HTML over the live DOM. The server
    // knows nothing about the trigger the dropdown built or the class hiding
    // the select underneath it, so a morph that trusted the server's
    // attributes wholesale unhid every select under its own trigger — one
    // dropdown drawn twice, per field, after a single save.
    //
    // The fix is not a longer exemption list. A node declares what belongs to
    // the client at the moment the client takes it, and the morph reads only
    // that. These assertions are the guard on that shape: reintroducing a
    // name into the morph is what makes them fail.
    let app = App::open().await;
    let admin = admin(&app).await;
    let page = app.get("/settings", Some(&admin)).await;
    let page = String::from_utf8_lossy(&page.bytes);

    assert!(
        page.contains("window.__izlekOwn = function (node, classes, attrs)"),
        "there is no way left for an enhancement to declare what it owns"
    );
    assert!(
        page.contains("window.__izlekAdded = function (node)"),
        "a client-built node can no longer say it is not a stray"
    );
    // The morph asks the node, never a list of names it was told to remember.
    assert!(
        page.contains("function clientMade(node) { return node.__izlekAdded === true; }"),
        "the stray test is back to sniffing class names"
    );
    assert!(
        page.contains("var own = from.__izlekMine"),
        "the attribute sweep no longer reads what the node owns"
    );
    // class is merged, not overwritten. This is the exact line the bug was.
    assert!(
        page.contains("syncClass(from, to, own)") && page.contains("own.c.forEach"),
        "the server's class list is overwriting the client's again"
    );

    // And the dropdown declares, rather than relying on the morph to know.
    assert!(
        page.contains("window.__izlekOwn(select, ['dd-native'], [])"),
        "the select's hidden state is no longer declared as the client's"
    );
    assert!(
        page.contains("window.__izlekAdded(trigger)")
            && page.contains("window.__izlekAdded(panel)"),
        "the trigger and panel no longer say they are the client's"
    );
}

#[tokio::test]
async fn a_rewired_dropdown_repairs_itself_instead_of_being_skipped() {
    // Ownership stops the morph from breaking the enhancement. It does not
    // make the enhancement follow the data: the trigger's label is drawn from
    // the select, so when a live refresh moves the selection the trigger has
    // to re-read it. An already-enhanced select is therefore repaired on
    // `izlek:wire`, not skipped.
    let app = App::open().await;
    let admin = admin(&app).await;
    let page = app.get("/settings", Some(&admin)).await;
    let page = String::from_utf8_lossy(&page.bytes);

    assert!(
        page.contains("if (select.dataset.ddDone) { resync(select); return; }"),
        "an already-enhanced select is skipped again instead of repaired"
    );
    assert!(
        page.contains("if (!rowsMatch(panel, select)) { fillRows(panel, select); }"),
        "the panel no longer follows the select's option list"
    );
    // Repairing under an open panel would move the rows out from under the
    // pointer; stale for that moment is the better failure.
    assert!(
        page.contains("if (panel.classList.contains('dd-open')) { return; }"),
        "the repair now runs under an open panel"
    );
}

#[tokio::test]
async fn the_parts_have_a_tab_and_a_subtask_is_not_offered_one() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let parent = a_task(&app, &admin, &columns[0], "Ship the exporter").await;
    let child = a_task(&app, &admin, &columns[0], "Write the CSV writer").await;
    app.store.set_parent(&child, Some(&parent)).await.unwrap();

    // The whole gets the tab, with the ratio on it.
    let whole = app.get(&format!("/?task={parent}"), Some(&admin)).await;
    let whole = String::from_utf8_lossy(&whole.bytes);
    assert!(
        whole.contains(&format!("/?task={parent}&amp;tab=subtasks")),
        "the parent has no Subtasks tab"
    );
    assert!(whole.contains("0/1"), "the tab does not carry the ratio");
    // ...and the parts are not on the Task tab any more.
    assert!(
        !whole.contains("subtask-list"),
        "the parts are still rendered under the task itself"
    );

    // The part is not offered a tab it can never fill.
    let part = app.get(&format!("/?task={child}"), Some(&admin)).await;
    let part = String::from_utf8_lossy(&part.bytes);
    assert!(
        !part.contains("tab=subtasks"),
        "a subtask was offered a Subtasks tab"
    );

    // And an address that names it anyway lands on the task, not on an empty
    // panel with no tab lit.
    let forced = app
        .get(&format!("/?task={child}&tab=subtasks"), Some(&admin))
        .await;
    let forced = String::from_utf8_lossy(&forced.bytes);
    assert!(
        !forced.contains("subtask-new"),
        "a subtask rendered the Subtasks tab"
    );
    assert!(
        forced.contains("detail-crumb-up"),
        "the fallback did not render the task itself"
    );
}

#[tokio::test]
async fn a_drop_decided_against_a_stale_board_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@izlek.sh", "Mo Dubois", Role::Member).await;
    let columns = columns_of(&app).await;
    let task = a_task(&app, &admin, &columns[0], "Contested").await;

    // Two people picked the same card up out of the first column.
    let first = app
        .post(
            "/api/move_card",
            Some(&admin),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(first.body, "");

    let second = app
        .post(
            "/api/move_card",
            Some(&member),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[2]),
            ],
        )
        .await;
    assert_eq!(second.body, "");
    assert!(
        second
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=moved-already&on=move_card"),
        "{:?}",
        second.location
    );

    // The winner's move stands, and there is exactly one crossing.
    let answer = app
        .post("/api/fetch_task", Some(&admin), &[("task_id", &task)])
        .await;
    assert_eq!(
        answer.body.matches("\"moved\"").count(),
        1,
        "the second drop wrote a crossing too: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_card_cannot_be_moved_into_another_boards_column() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let task = a_task(&app, &admin, &columns[0], "Stays put").await;

    let answer = app
        .post(
            "/api/move_card",
            Some(&admin),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", "00000000-0000-0000-0000-000000000000"),
            ],
        )
        .await;
    assert_eq!(answer.body, "");
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=move_card"),
        "{:?}",
        answer.location
    );
    let answer = app
        .post("/api/fetch_task", Some(&admin), &[("task_id", &task)])
        .await;
    assert!(
        answer.body.contains(&columns[0]),
        "the forbidden move happened anyway"
    );
}

// ---------------------------------------------------------------------------
// Settings: sign-out, profile, limits, sender, test mail, resend
// ---------------------------------------------------------------------------

/// Settings has three `select.field-input`s (timezone, theme, language) plus
/// a member-role `select.status-select` once there are members — the page
/// carries no board.rs, so it has to wire `dropdown.rs`'s script in for
/// itself; this proves it did, and that the theme select still posts as a
/// plain named field.
#[tokio::test]
async fn settings_selects_keep_their_hidden_form_fields_and_get_a_dropdown_trigger() {
    let app = App::open().await;
    let cookie = admin(&app).await;

    let page = app.get("/settings", Some(&cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains("select class=\"field-input\" name=\"theme\""),
        "{html}"
    );
    assert!(
        html.contains("dd-trigger"),
        "no dropdown script on the settings page: {html}"
    );
    assert!(
        html.contains("__izlekEsc.register(40"),
        "no escape script on the settings page: {html}"
    );
    assert!(
        html.contains("closest('.user-menu')"),
        "no user-menu close path on the settings page: {html}"
    );
}

/// The rules and logs pages carry no `board.rs`, so like settings they wire
/// `layout.rs`'s shared Escape script in for themselves — rules also closes
/// its `<details class="rule-new">` composer on the same press.
#[tokio::test]
async fn the_rules_and_logs_pages_get_the_shared_escape_script() {
    let app = App::open().await;
    let cookie = admin(&app).await;

    let rules_page = app.get("/rules", Some(&cookie)).await;
    let rules_html = String::from_utf8_lossy(&rules_page.bytes);
    assert!(
        rules_html.contains("__izlekEsc.register(40"),
        "no escape script on the rules page: {rules_html}"
    );
    assert!(
        rules_html.contains("details.rule-new[open]"),
        "escape script does not close the rule composer: {rules_html}"
    );

    let logs_page = app.get("/logs", Some(&cookie)).await;
    let logs_html = String::from_utf8_lossy(&logs_page.bytes);
    assert!(
        logs_html.contains("__izlekEsc.register(40"),
        "no escape script on the logs page: {logs_html}"
    );
}

/// The soft swap settles the address bar before it runs the swapped-in
/// page's scripts, never after. `logs.rs`'s fit script reloads through
/// `location.replace(location.href)`; while the link handler pushed the
/// new URL only after `swap()` had re-executed scripts, that read the
/// page the browser had just left, so clicking Logs painted the page and
/// hard-navigated back to the board a few hundred milliseconds later.
/// Ordering inside `swap` is the whole fix, so ordering is what is
/// asserted, together with the hazard that makes it matter.
#[tokio::test]
async fn the_soft_swap_rewrites_the_url_before_the_new_pages_scripts_run() {
    let app = App::open().await;
    let cookie = admin(&app).await;

    let page = app.get("/", Some(&cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    let (_, after) = html
        .split_once("function swap(html, url, fresh, push, morphing)")
        .unwrap_or_else(|| panic!("no soft-nav swap on the board page: {html}"));
    let body = after
        .split_once("window.__izlekGo")
        .unwrap_or_else(|| panic!("swap runs past the end of the script: {after}"))
        .0;
    let rewrite = body
        .find("history[")
        .unwrap_or_else(|| panic!("swap no longer rewrites history: {body}"));
    let run = body
        .find("document.body.replaceChildren()")
        .unwrap_or_else(|| panic!("swap no longer replaces the body: {body}"));
    assert!(
        rewrite < run,
        "swap runs the new page's scripts before the URL is settled: {body}"
    );

    // Every caller hands `swap` the response URL. A `null` here is the
    // old shape: the address bar lagged the body by one navigation.
    assert!(
        html.contains("swap(t, r.url, true, true)"),
        "the link handler no longer pushes through swap: {html}"
    );
    assert!(
        !html.contains("swap(t, null"),
        "a swap still runs the new page's scripts under the old URL: {html}"
    );

    // The hazard itself: a swapped-in script that reads `location.href`
    // on load. Were this to go, the ordering above would still be right
    // but would guard nothing.
    let logs = app.get("/logs", Some(&cookie)).await;
    let logs_html = String::from_utf8_lossy(&logs.bytes);
    assert!(
        logs_html.contains("location.replace(location.href)"),
        "the log-fit reload is gone: {logs_html}"
    );
}

#[tokio::test]
async fn signing_out_stops_the_session_being_worth_anything() {
    let app = App::open().await;
    let cookie = admin(&app).await;

    let before = app.get("/settings", Some(&cookie)).await;
    let html = String::from_utf8_lossy(&before.bytes);
    assert!(html.contains("Sign out"), "{html}");

    let out = app.post("/api/sign_out", Some(&cookie), &[]).await;
    assert_eq!(out.status, StatusCode::SEE_OTHER, "{}", out.body);

    // The same cookie, replayed. The server has to be the one refusing.
    let after = app.get("/settings", Some(&cookie)).await;
    let html = String::from_utf8_lossy(&after.bytes);
    assert!(
        !html.contains("Sign out"),
        "the session outlived signing out: {html}"
    );
}

#[tokio::test]
async fn signing_out_without_script_lands_on_the_sign_in_page() {
    let app = App::open().await;
    let cookie = admin(&app).await;

    let out = app
        .post_without_script(
            "/api/sign_out",
            Some(&cookie),
            "http://izlek.test/settings",
            &[],
        )
        .await;

    assert_eq!(out.status, StatusCode::SEE_OTHER, "{}", out.body);
    assert_eq!(out.location.as_deref(), Some("/"), "{:?}", out.location);
}

#[tokio::test]
async fn signing_out_leaves_every_other_session_alone() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let out = app.post("/api/sign_out", Some(&member), &[]).await;
    assert_eq!(out.status, StatusCode::SEE_OTHER, "{}", out.body);

    let still = app.get("/settings", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&still.bytes);
    assert!(html.contains("Sign out"), "{html}");
}

#[tokio::test]
async fn saving_a_profile_renames_the_person_asking_and_nobody_else() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&member),
            &[("display_name", "Emre Y")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        answer.location
    );

    let mine = app.get("/settings", Some(&member)).await;
    let html = String::from_utf8_lossy(&mine.bytes);
    assert!(html.contains("Emre Y"), "{html}");

    let theirs = app.get("/settings", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&theirs.bytes);
    assert!(html.contains("Ada Lovelace"), "{html}");
}

#[tokio::test]
async fn a_profile_cannot_be_saved_without_a_name() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "   ")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=empty-name&on=save_profile"),
        "{location}"
    );

    let page = app.get(location, Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("Give yourself a name."), "{html}");
}

#[tokio::test]
async fn a_profile_can_change_its_own_email_and_stays_signed_in() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&member),
            &[("display_name", "Emre Y"), ("email", "emre.new@izlek.sh")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        answer.location
    );

    // Same cookie still works: the session is keyed by user id, not address.
    let mine = app.get("/settings", Some(&member)).await;
    assert_eq!(mine.status, StatusCode::OK, "{}", mine.status);
    let html = String::from_utf8_lossy(&mine.bytes);
    assert!(html.contains("emre.new@izlek.sh"), "{html}");
}

#[tokio::test]
async fn a_profile_email_cannot_take_somebody_elses_address() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&member),
            &[("display_name", "Emre"), ("email", "ada@izlek.sh")],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=address-taken&on=save_profile"),
        "{location}"
    );

    let page = app.get(location, Some(&member)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("already has an account"), "{html}");
}

// A taken email used to refuse only the email while the name/theme/language/
// timezone in the same form still got written — a half-applied save.
#[tokio::test]
async fn a_taken_email_refuses_the_whole_save_not_only_the_email() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&member),
            &[
                ("display_name", "Emre Renamed"),
                ("email", "ada@izlek.sh"),
                ("theme", "dark"),
            ],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=address-taken&on=save_profile"),
        "{location}"
    );

    let mine = app.get("/settings", Some(&member)).await;
    let html = String::from_utf8_lossy(&mine.bytes);
    assert!(
        html.contains("value=\"Emre\""),
        "the name changed anyway: {html}"
    );
}

#[tokio::test]
async fn a_signed_out_browser_cannot_rename_anybody() {
    let app = App::open().await;
    let _ = admin(&app).await;

    let answer = app
        .post("/api/save_profile", None, &[("display_name", "Whoever")])
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=sign-in-first&on=save_profile"),
        "{:?}",
        answer.location
    );
}

#[tokio::test]
async fn a_member_who_posts_new_limits_anyway_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/save_limits",
            Some(&member),
            &[
                ("attachment_limit_mb", "400"),
                ("photo_limit_mb", "19"),
                ("allowed_file_types", "png"),
                ("mail_batch_minutes", "5"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=save_limits"),
        "{:?}",
        answer.location
    );

    // The refusal is not cosmetic: the limits are where they were.
    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.attachment_limit_bytes, 25 * 1024 * 1024);
}

#[tokio::test]
async fn an_admin_changes_the_limits_and_they_stay_changed() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "10"),
                ("photo_limit_mb", "1"),
                ("allowed_file_types", ".PNG, png, pdf"),
                ("mail_batch_minutes", "5"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("saved=save_limits"),
        "{:?}",
        answer.location
    );

    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.attachment_limit_bytes, 10 * 1024 * 1024);
    assert_eq!(workspace.photo_limit_bytes, 1024 * 1024);
    assert_eq!(
        workspace.allowed_file_types,
        vec!["png".to_string(), "pdf".to_string()]
    );
}

#[tokio::test]
async fn a_limit_outside_what_the_disk_should_promise_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    for (attachment, photo) in [("5000", "2"), ("0", "2"), ("25", "0"), ("25", "200")] {
        let answer = app
            .post(
                "/api/save_limits",
                Some(&admin_cookie),
                &[
                    ("attachment_limit_mb", attachment),
                    ("photo_limit_mb", photo),
                    ("allowed_file_types", ""),
                    ("mail_batch_minutes", "5"),
                ],
            )
            .await;
        let location = answer.location.as_deref().unwrap_or_default();
        assert!(
            location.contains("refusal=bad-limit&on=save_limits"),
            "{attachment}/{photo}: {location}"
        );

        let page = app.get(location, Some(&admin_cookie)).await;
        let html = String::from_utf8_lossy(&page.bytes);
        assert!(
            html.contains("A limit has to be at least 1 MB, and no wider than 500 MB per file or 20 MB per photo."),
            "{attachment}/{photo}: {html}"
        );
    }
}

#[tokio::test]
async fn a_file_type_that_is_not_an_extension_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "25"),
                ("photo_limit_mb", "2"),
                ("allowed_file_types", "../etc/passwd"),
                ("mail_batch_minutes", "5"),
            ],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-file-type&on=save_limits"),
        "{location}"
    );

    let page = app.get(location, Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("File types are extensions"), "{html}");
}

#[tokio::test]
async fn only_an_admin_may_write_the_sender() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/save_sender",
            Some(&member),
            &[
                ("host", "smtp.attacker.example"),
                ("port", "587"),
                ("username", "emre"),
                ("password", "let-me-in"),
                ("from_name", "İzlek"),
                ("from_address", "emre@izlek.sh"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=save_sender"),
        "{:?}",
        answer.location
    );

    // And nothing was written.
    let workspace = app.store.workspace().await.unwrap();
    assert!(
        workspace.and_then(|ws| ws.smtp_host).unwrap_or_default() != "smtp.attacker.example",
        "the attacker's host was stored"
    );
}

#[tokio::test]
async fn a_signed_out_browser_may_not_write_the_sender() {
    let app = App::open().await;
    let _admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_sender",
            None,
            &[
                ("host", "smtp.attacker.example"),
                ("port", "587"),
                ("username", "nobody"),
                ("password", "let-me-in"),
                ("from_name", "İzlek"),
                ("from_address", "nobody@izlek.sh"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=sign-in-first&on=save_sender"),
        "{:?}",
        answer.location
    );

    let workspace = app.store.workspace().await.unwrap();
    assert!(workspace.and_then(|ws| ws.smtp_host).unwrap_or_default() != "smtp.attacker.example");
}

#[tokio::test]
async fn an_edit_with_no_password_typed_keeps_the_stored_one() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);

    let answer = app
        .post(
            "/api/save_sender",
            Some(&admin_cookie),
            &[
                ("host", "smtp.fastmail.com"),
                ("port", "587"),
                ("username", "izlek"),
                ("password", ""),
                ("from_name", "İzlek"),
                ("from_address", "izlek@izlek.sh"),
            ],
        )
        .await;
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "an empty password field was refused: {:?}",
        answer.location
    );

    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.smtp_port, Some(587));
    assert!(
        workspace.smtp_password_set,
        "an empty field blanked the password"
    );
}

#[tokio::test]
async fn a_first_sender_with_no_password_is_refused_and_says_why() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_sender",
            Some(&admin_cookie),
            &[
                ("host", "smtp.fastmail.com"),
                ("port", "587"),
                ("username", "izlek"),
                ("password", ""),
                ("from_name", "İzlek"),
                ("from_address", "izlek@izlek.sh"),
            ],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-sender&on=save_sender"),
        "{location}"
    );

    let page = app.get(location, Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains("A password is needed the first time."),
        "{html}"
    );
}

#[tokio::test]
async fn a_sender_field_that_cannot_work_is_refused_by_name() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let bad: &[(&str, &[(&str, &str)], &str)] = &[
        ("host", &[("host", "  ")], "Give the SMTP host."),
        (
            "host",
            &[("host", "smtp.fastmail.com/inbox")],
            "The SMTP host is a host name, not an address or a URL.",
        ),
        (
            "port",
            &[("port", "0")],
            "A port is a number between 1 and 65535.",
        ),
        (
            "port",
            &[("port", "99999")],
            "A port is a number between 1 and 65535.",
        ),
        ("username", &[("username", "")], "Give the SMTP username."),
        (
            "from_address",
            &[("from_address", "board-at-izlek")],
            "That is not a from-address.",
        ),
        (
            "from_address",
            &[("from_address", "board@izlek")],
            "That is not a from-address.",
        ),
    ];
    for (field, overrides, expected) in bad {
        let mut form: Vec<(&str, &str)> = vec![
            ("host", "smtp.fastmail.com"),
            ("port", "587"),
            ("username", "izlek"),
            ("password", SENDER_PASSWORD),
            ("from_name", "İzlek"),
            ("from_address", "izlek@izlek.sh"),
        ];
        for (key, value) in *overrides {
            for slot in form.iter_mut() {
                if slot.0 == *key {
                    slot.1 = value;
                }
            }
        }
        let answer = app
            .post("/api/save_sender", Some(&admin_cookie), &form)
            .await;
        let location = answer.location.as_deref().unwrap_or_default();
        assert!(
            location.contains("refusal=bad-sender&on=save_sender"),
            "{field} was accepted with {overrides:?}: {location}"
        );

        let page = app.get(location, Some(&admin_cookie)).await;
        let html = String::from_utf8_lossy(&page.bytes);
        assert!(html.contains(*expected), "{field}: {html}");
    }
}

#[tokio::test]
async fn a_member_may_not_press_the_test_button() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app.post("/api/send_test_mail", Some(&member), &[]).await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=send_test_mail"),
        "{:?}",
        answer.location
    );
}

#[tokio::test]
async fn a_viewer_may_not_press_the_test_button() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let viewer = invited(&app, &admin_cookie, "pinar@izlek.sh", "Pinar", Role::Viewer).await;

    let answer = app.post("/api/send_test_mail", Some(&viewer), &[]).await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=send_test_mail"),
        "{:?}",
        answer.location
    );
}

#[tokio::test]
async fn a_signed_out_browser_may_not_press_the_test_button() {
    let app = App::open().await;

    let answer = app.post("/api/send_test_mail", None, &[]).await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=sign-in-first&on=send_test_mail"),
        "{:?}",
        answer.location
    );
}

#[tokio::test]
async fn testing_a_sender_that_was_never_filled_in_says_so_rather_than_sending() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post("/api/send_test_mail", Some(&admin_cookie), &[])
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=bad-sender&on=send_test_mail"),
        "{:?}",
        answer.location
    );

    // Nothing was recorded either: the panel still has no test line to show.
    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert!(
        workspace.sender_test.is_none(),
        "a test with no sender recorded a result"
    );
}

#[tokio::test]
async fn a_member_who_posts_a_resend_anyway_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let mert = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", "mert@izlek.sh"),
                ("display_name", "Mert"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(mert.status, StatusCode::OK, "{}", mert.body);
    let workspace_id = app.workspace_id().await;
    let mert_id = app
        .store
        .users(&workspace_id)
        .await
        .unwrap()
        .into_iter()
        .find(|user| user.email == "mert@izlek.sh")
        .expect("no member row for mert")
        .id;

    let answer = app
        .post("/api/resend_link", Some(&member), &[("user_id", &mert_id)])
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=resend_link"),
        "{:?}",
        answer.location
    );
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("mailed="),
        "{:?}",
        answer.location
    );
}

#[tokio::test]
async fn a_resent_link_opens_the_same_account() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let invitation = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", "mert@izlek.sh"),
                ("display_name", "Mert"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(invitation.status, StatusCode::OK, "{}", invitation.body);
    let workspace_id = app.workspace_id().await;
    let mert_id = app
        .store
        .users(&workspace_id)
        .await
        .unwrap()
        .into_iter()
        .find(|user| user.email == "mert@izlek.sh")
        .expect("no member row for mert")
        .id;

    let answer = app
        .post(
            "/api/resend_link",
            Some(&admin_cookie),
            &[("user_id", &mert_id)],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("mailed=mert%40izlek.sh"),
        "{:?}",
        answer.location
    );
    let token = queued_join_token(&app, "mert@izlek.sh").await;

    let redeemed = app
        .post(
            "/api/redeem_link",
            None,
            &[
                ("token", &token),
                ("password", "lantern gravel spoon meadow"),
            ],
        )
        .await;
    assert_eq!(redeemed.status, StatusCode::SEE_OTHER, "{}", redeemed.body);
    assert_eq!(redeemed.body, "null", "{}", redeemed.body);
    assert!(
        redeemed.session.is_some(),
        "the resent link signed nobody in"
    );
}

#[tokio::test]
async fn an_admin_may_change_a_members_role_over_http() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let mert = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", "mert@izlek.sh"),
                ("display_name", "Mert"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(mert.status, StatusCode::OK, "{}", mert.body);
    let workspace_id = app.workspace_id().await;
    let mert_id = app
        .store
        .users(&workspace_id)
        .await
        .unwrap()
        .into_iter()
        .find(|user| user.email == "mert@izlek.sh")
        .expect("no member row for mert")
        .id;

    let answer = app
        .post(
            "/api/set_role",
            Some(&admin_cookie),
            &[("user_id", &mert_id), ("role", "viewer")],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("saved=set_role"),
        "{:?}",
        answer.location
    );

    let reloaded = app.store.user(&mert_id).await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Viewer, "role change did not persist");
}

#[tokio::test]
async fn the_owner_cannot_be_retargeted_over_http() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let owner_id = app
        .store
        .owner()
        .await
        .unwrap()
        .expect("workspace has no owner")
        .id;

    let answer = app
        .post(
            "/api/set_role",
            Some(&admin_cookie),
            &[("user_id", &owner_id), ("role", "member")],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=set_role"),
        "{:?}",
        answer.location
    );

    let reloaded = app.store.user(&owner_id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.role,
        Role::Admin,
        "the owner's role changed anyway"
    );
}

#[tokio::test]
async fn a_non_admin_may_not_set_roles_over_http() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let mert = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", "mert@izlek.sh"),
                ("display_name", "Mert"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(mert.status, StatusCode::OK, "{}", mert.body);
    let workspace_id = app.workspace_id().await;
    let mert_id = app
        .store
        .users(&workspace_id)
        .await
        .unwrap()
        .into_iter()
        .find(|user| user.email == "mert@izlek.sh")
        .expect("no member row for mert")
        .id;

    let answer = app
        .post(
            "/api/set_role",
            Some(&member),
            &[("user_id", &mert_id), ("role", "admin")],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=set_role"),
        "{:?}",
        answer.location
    );

    let reloaded = app.store.user(&mert_id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.role,
        Role::Member,
        "role change was not really refused"
    );
}

#[tokio::test]
async fn redeeming_a_link_lands_on_the_board_not_the_spent_join_page() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    app.post(
        "/api/invite_member",
        Some(&admin_cookie),
        &[
            ("email", "asli@izlek.sh"),
            ("display_name", "Asli"),
            ("role", "member"),
        ],
    )
    .await;
    let token = queued_join_token(&app, "asli@izlek.sh").await;

    let answer = app
        .post_without_script(
            "/api/redeem_link",
            None,
            &format!("http://izlek.test/join/{token}"),
            &[
                ("token", &token),
                ("password", "lantern gravel spoon meadow"),
            ],
        )
        .await;
    assert_eq!(
        answer.location.as_deref(),
        Some("/"),
        "{:?}",
        answer.location
    );
}

#[tokio::test]
async fn an_invitation_names_the_admin_who_made_it_and_not_the_invitee() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", "grace@izlek.sh"),
                ("display_name", "Grace Hopper"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let token = queued_join_token(&app, "grace@izlek.sh").await;

    let answer = app
        .post("/api/invitation", None, &[("token", token.as_str())])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(
        answer.body.contains(r#""invited_by":"Ada Lovelace""#),
        "the invitation does not name the admin: {}",
        answer.body
    );
    assert!(
        answer.body.contains(r#""display_name":"Grace Hopper""#),
        "the invitation lost the invitee: {}",
        answer.body
    );
}

// ---------------------------------------------------------------------------
// Settings visibility: the sender and the member list are the admin's
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_signed_out_browser_is_told_nothing_by_the_settings_call() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);

    let page = app.get("/settings", None).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("Sign in first."), "{html}");
    assert!(!html.contains("fastmail"), "{html}");
}

#[tokio::test]
async fn a_member_is_told_nothing_about_the_sender() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let page = app.get("/settings", Some(&member)).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(!html.contains("Outgoing mail"), "{html}");
    assert!(!html.contains("fastmail"), "{html}");
}

#[tokio::test]
async fn an_admin_sees_the_sender_and_never_a_password() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);

    let page = app
        .get("/settings?section=outgoing", Some(&admin_cookie))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("smtp.fastmail.com"), "{html}");
    assert!(html.contains("izlek@izlek.sh"), "{html}");
    // Saved in full, and nobody has dialled the server — this router has no
    // mail engine at all. "Connected" would be a claim about a handshake that
    // never happened, which is the whole reason the chip has four states.
    assert!(html.contains("Unchecked"), "{html}");
    assert!(!html.contains("chip-connected"), "{html}");
    assert!(
        !html.contains(SENDER_PASSWORD),
        "the settings page carried the SMTP password: {html}"
    );
}

#[tokio::test]
async fn a_member_is_not_sent_the_member_list() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let _ = invited(&app, &admin_cookie, "quiet@izlek.sh", "Quiet", Role::Viewer).await;

    let page = app.get("/settings", Some(&member)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(!html.contains("member-table"), "{html}");
    assert!(!html.contains("quiet@izlek.sh"), "{html}");
}

#[tokio::test]
async fn an_admin_sees_who_has_a_password_and_never_a_hash() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let _ = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    // Invited and never signed in: the account exists, the password does not.
    let answer = app
        .post(
            "/api/invite_member",
            Some(&admin_cookie),
            &[
                ("email", "mert@izlek.sh"),
                ("display_name", "Mert"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);

    let page = app
        .get("/settings?section=members", Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("mert@izlek.sh"), "{html}");
    assert!(
        html.contains("Resend mail"),
        "the un-signed-in member has no resend control: {html}"
    );
    assert!(!html.contains("$argon2"), "a hash reached the page: {html}");
}

// ---------------------------------------------------------------------------
// Mail rules
// ---------------------------------------------------------------------------

/// Writes one rule as the admin, and reports whether the server took it.
async fn rule_written(app: &App, admin: &str, column_id: &str, subject: &str) -> bool {
    let answer = app
        .post(
            "/api/create_rule",
            Some(admin),
            &[
                ("trigger", "status"),
                ("column_id", column_id),
                ("subject", subject),
                ("audience", "assignees"),
            ],
        )
        .await;
    answer.status == StatusCode::SEE_OTHER && answer.body == "null"
}

/// The id of the one rule the workspace has.
async fn only_rule(app: &App, admin: &str) -> String {
    let answer = app.post("/api/current_rules", Some(admin), &[]).await;
    answer
        .body
        .split("\"rules\":[")
        .nth(1)
        .and_then(|rest| rest.split("\"id\":\"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no rule id in {}", answer.body))
        .to_string()
}

#[tokio::test]
async fn an_admin_writes_a_rule_and_reads_it_back_as_a_sentence() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;

    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);

    let answer = app
        .post("/api/current_rules", Some(&admin_cookie), &[])
        .await;
    assert!(
        answer.body.contains("\"when\":\"When status becomes\""),
        "{}",
        answer.body
    );
    assert!(
        answer.body.contains("\"subject\":\"Task completed\""),
        "{}",
        answer.body
    );
    assert!(
        answer.body.contains("\"audience\":\"assignees\""),
        "{}",
        answer.body
    );
    // Nothing has been sent, and the row says so rather than nothing at all.
    assert!(
        answer.body.contains("\"last_sent\":null"),
        "{}",
        answer.body
    );
}

#[tokio::test]
async fn a_rule_with_no_subject_is_refused_and_says_why() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;

    let answer = app
        .post(
            "/api/create_rule",
            Some(&admin_cookie),
            &[
                ("trigger", "status"),
                ("column_id", &column),
                ("subject", "   "),
                ("audience", "assignees"),
            ],
        )
        .await;
    assert!(answer.body.contains("EmptySubject"), "{}", answer.body);
}

#[tokio::test]
async fn a_trigger_with_nothing_to_name_prints_no_term_box() {
    // "When someone is assigned" names no column, and the sentence used to
    // print the term box anyway, with nothing in it — an empty rounded box
    // sitting between the words. The absence was carried as an empty string,
    // so the renderer could not tell "no term" from "a term that is blank".
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;

    // One rule that has a term, and one that has none.
    app.post(
        "/api/create_rule",
        Some(&admin_cookie),
        &[
            ("trigger", "status"),
            ("column_id", &column),
            ("subject", "A card reached the column."),
            ("audience", "assignees"),
        ],
    )
    .await;
    app.post(
        "/api/create_rule",
        Some(&admin_cookie),
        &[
            ("trigger", "assigned"),
            ("column_id", ""),
            ("subject", "You have been assigned a task."),
            ("audience", "assignees"),
        ],
    )
    .await;

    let page = app.get("/rules", Some(&admin_cookie)).await;
    let page = String::from_utf8_lossy(&page.bytes);
    // The rule with no column is on the page at all — without this the
    // assertion below passes by having nothing to assert on.
    assert!(
        page.contains("When someone is assigned"),
        "the triggerless rule was never created, so this test proves nothing"
    );
    assert!(
        !page.contains(r#"<span class="rule-term"></span>"#),
        "a trigger with no second half still prints an empty term box"
    );
    // The rule that does name a column still shows it, so this did not fix
    // the box by removing it.
    assert!(
        page.contains(r#"<span class="rule-term">"#),
        "the term box is gone from the rule that has a term"
    );
}

#[tokio::test]
async fn a_rule_may_not_be_hung_off_a_column_that_is_not_on_this_board() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/create_rule",
            Some(&admin_cookie),
            &[
                ("trigger", "status"),
                ("column_id", &Ulid::new().to_string()),
                ("subject", "Task completed"),
                ("audience", "assignees"),
            ],
        )
        .await;
    assert!(answer.body.contains("Forbidden"), "{}", answer.body);

    let seen = app
        .post("/api/current_rules", Some(&admin_cookie), &[])
        .await;
    assert!(seen.body.contains("\"rules\":[]"), "{}", seen.body);
}

#[tokio::test]
async fn an_audience_the_screen_never_offers_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;

    let answer = app
        .post(
            "/api/create_rule",
            Some(&admin_cookie),
            &[
                ("trigger", "status"),
                ("column_id", &column),
                ("subject", "Task completed"),
                ("audience", "everyone-everywhere"),
            ],
        )
        .await;
    assert!(answer.body.contains("Forbidden"), "{}", answer.body);
}

#[tokio::test]
async fn switching_a_rule_off_leaves_it_listed_and_switched_off() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);
    let rule = only_rule(&app, &admin_cookie).await;

    let answer = app
        .post(
            "/api/set_rule_enabled",
            Some(&admin_cookie),
            &[("rule_id", &rule), ("enabled", "false")],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    // The screen lists what exists, not what is live.
    let seen = app
        .post("/api/current_rules", Some(&admin_cookie), &[])
        .await;
    assert!(seen.body.contains("\"enabled\":false"), "{}", seen.body);
    assert!(
        seen.body.contains("\"subject\":\"Task completed\""),
        "{}",
        seen.body
    );
}

#[tokio::test]
async fn a_rule_id_this_workspace_does_not_own_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let stranger = Ulid::new().to_string();

    for path in ["/api/set_rule_enabled", "/api/delete_rule"] {
        let answer = app
            .post(
                path,
                Some(&admin_cookie),
                &[("rule_id", &stranger), ("enabled", "false")],
            )
            .await;
        assert!(answer.body.contains("NotFound"), "{path}: {}", answer.body);
    }
}

#[tokio::test]
async fn deleting_a_rule_takes_it_off_the_screen() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);
    let rule = only_rule(&app, &admin_cookie).await;

    let answer = app
        .post(
            "/api/delete_rule",
            Some(&admin_cookie),
            &[("rule_id", &rule)],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    let seen = app
        .post("/api/current_rules", Some(&admin_cookie), &[])
        .await;
    assert!(seen.body.contains("\"rules\":[]"), "{}", seen.body);
}

#[tokio::test]
async fn only_an_admin_may_read_or_write_the_rules() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);
    let rule = only_rule(&app, &admin_cookie).await;

    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let viewer = invited(&app, &admin_cookie, "pinar@izlek.sh", "Pinar", Role::Viewer).await;

    for who in [&member, &viewer] {
        let read = app.post("/api/current_rules", Some(who), &[]).await;
        assert!(read.body.contains("Forbidden"), "{}", read.body);

        let written = app
            .post(
                "/api/create_rule",
                Some(who),
                &[
                    ("trigger", "status"),
                    ("column_id", &column),
                    ("subject", "Mail everyone about me"),
                    ("audience", "board"),
                ],
            )
            .await;
        assert!(written.body.contains("Forbidden"), "{}", written.body);

        let switched = app
            .post(
                "/api/set_rule_enabled",
                Some(who),
                &[("rule_id", &rule), ("enabled", "false")],
            )
            .await;
        assert!(switched.body.contains("Forbidden"), "{}", switched.body);

        let deleted = app
            .post("/api/delete_rule", Some(who), &[("rule_id", &rule)])
            .await;
        assert!(deleted.body.contains("Forbidden"), "{}", deleted.body);
    }

    // And the rule is untouched: still there, still on.
    let seen = app
        .post("/api/current_rules", Some(&admin_cookie), &[])
        .await;
    assert!(seen.body.contains("\"enabled\":true"), "{}", seen.body);
    assert!(
        !seen.body.contains("Mail everyone about me"),
        "{}",
        seen.body
    );
}

#[tokio::test]
async fn a_signed_out_browser_may_not_touch_the_rules() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);
    let rule = only_rule(&app, &admin_cookie).await;

    for (path, form) in [
        ("/api/current_rules", vec![] as Vec<(&str, &str)>),
        (
            "/api/create_rule",
            vec![
                ("trigger", "status"),
                ("column_id", column.as_str()),
                ("subject", "Task completed"),
                ("audience", "assignees"),
            ],
        ),
        (
            "/api/set_rule_enabled",
            vec![("rule_id", rule.as_str()), ("enabled", "false")],
        ),
        ("/api/delete_rule", vec![("rule_id", rule.as_str())]),
    ] {
        let answer = app.post(path, None, &form).await;
        assert!(
            answer.body.contains("SignInFirst"),
            "{path}: {}",
            answer.body
        );
    }
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_download_of_an_unknown_attachment_is_not_found_and_signed_out_is_a_redirect() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let signed_in = app.get("/files/anything", Some(&admin_cookie)).await;
    assert_eq!(signed_in.status, StatusCode::NOT_FOUND);

    let signed_out = app.get("/files/anything", None).await;
    assert_eq!(signed_out.status, StatusCode::SEE_OTHER);
}

// A signed-in member's detail page carries the upload form no-script needs: a
// real multipart `<form>` posting to `/files`, not a hydrated one — a browser
// with no script still has a way to attach a file.
#[tokio::test]
async fn the_files_section_is_on_the_detail_page() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Attach something to me").await;

    let page = app
        .get(&format!("/?task={task}&tab=files"), Some(&admin_cookie))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains("multipart/form-data"),
        "no multipart upload form on the detail page"
    );
    assert!(
        html.contains(r#"action="/files""#),
        "the upload form does not post to /files"
    );
}

#[tokio::test]
async fn the_datepicker_shell_renders_in_both_task_modals() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Pick a date on me").await;

    let detail = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&detail.bytes);
    assert!(
        html.contains("datepick-input"),
        "no datepicker shell on the task modal: {html}"
    );
    assert!(
        html.contains("datepick-grid"),
        "no datepicker grid on the task modal: {html}"
    );

    let new_task = app.get("/?new=1", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&new_task.bytes);
    assert!(
        html.contains("datepick-input"),
        "no datepicker shell on the new-task modal: {html}"
    );
    assert!(
        html.contains("datepick-grid"),
        "no datepicker grid on the new-task modal: {html}"
    );
}

/// The id of the file named `name` in a `/api/fetch_task` snapshot's body,
/// read the way the page would: found by its name, then the `id` field that
/// comes right before it on the wire.
fn attachment_id_named(body: &str, name: &str) -> String {
    let needle = format!("\",\"name\":\"{name}\"");
    let before = body
        .split_once(&needle)
        .map(|(head, _)| head)
        .unwrap_or_else(|| panic!("no file named {name} in the detail snapshot: {body}"));
    before
        .rsplit_once("\"id\":\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no id before the file name")
        .to_string()
}

#[tokio::test]
async fn a_viewer_who_posts_an_upload_anyway_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let viewer = invited(
        &app,
        &admin_cookie,
        "quiet@izlek.sh",
        "Quiet Reader",
        Role::Viewer,
    )
    .await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Viewers cannot attach").await;

    let answer = app
        .post_multipart(
            "/files",
            Some(&viewer),
            &[("task_id", &task)],
            Some(("note.txt", "text/plain", b"hello")),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/?refusal=forbidden&on=upload_file")
    );
}

#[tokio::test]
async fn a_member_uploads_a_file_and_the_chip_comes_back() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(
        &app,
        &admin_cookie,
        "mo@izlek.sh",
        "Mo Dubois",
        Role::Member,
    )
    .await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Attach the spec").await;

    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    let answer = app
        .post_multipart(
            "/files",
            Some(&member),
            &[("task_id", &task)],
            Some(("spec.png", "image/png", &png)),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some(format!("/?task={task}&tab=files").as_str())
    );

    let snapshot = app
        .post("/api/fetch_task", Some(&member), &[("task_id", &task)])
        .await;
    let file_id = attachment_id_named(&snapshot.body, "spec.png");

    let page = app
        .get(&format!("/?task={task}&tab=files"), Some(&member))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(&format!("task={task}&amp;tab=files&amp;file={file_id}")),
        "no viewer href for the new file: {html}"
    );
    assert!(html.contains("spec.png"));

    let download = app.get(&format!("/files/{file_id}"), Some(&member)).await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.bytes, png);
}

#[tokio::test]
async fn an_image_downloads_inline_and_an_unrecognised_type_stays_attachment() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Two kinds of file").await;

    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some(("spec.png", "image/png", &png)),
    )
    .await;
    let unknown = [0x00u8, 0x01, 0xFE, 0xFF, 0x02];
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some(("blob.bin", "application/octet-stream", &unknown)),
    )
    .await;

    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let png_id = attachment_id_named(&snapshot.body, "spec.png");
    let blob_id = attachment_id_named(&snapshot.body, "blob.bin");

    let png_download = app
        .get(&format!("/files/{png_id}"), Some(&admin_cookie))
        .await;
    assert_eq!(png_download.content_type.as_deref(), Some("image/png"));
    assert!(
        png_download
            .disposition
            .as_deref()
            .unwrap_or_default()
            .starts_with("inline;"),
        "{:?}",
        png_download.disposition
    );

    let blob_download = app
        .get(&format!("/files/{blob_id}"), Some(&admin_cookie))
        .await;
    assert!(
        blob_download
            .disposition
            .as_deref()
            .unwrap_or_default()
            .starts_with("attachment;"),
        "{:?}",
        blob_download.disposition
    );
}

#[tokio::test]
async fn dl_forces_a_download_of_an_otherwise_inline_type() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Forced download").await;

    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some(("spec.png", "image/png", &png)),
    )
    .await;
    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "spec.png");

    let forced = app
        .get(&format!("/files/{file_id}?dl=1"), Some(&admin_cookie))
        .await;
    assert!(
        forced
            .disposition
            .as_deref()
            .unwrap_or_default()
            .starts_with("attachment;"),
        "{:?}",
        forced.disposition
    );
}

// Safari refuses to play `<video>`/`<audio>` without a `206` answer to its
// own `Range` probe on the download route.
#[tokio::test]
async fn a_range_request_answers_206_with_the_sliced_bytes_and_a_bad_range_answers_416() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ranged download").await;

    let bytes = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some(("spec.png", "image/png", &bytes)),
    )
    .await;
    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "spec.png");
    let total = bytes.len();

    let no_range = app
        .get(&format!("/files/{file_id}"), Some(&admin_cookie))
        .await;
    assert_eq!(no_range.status, StatusCode::OK);
    assert_eq!(no_range.accept_ranges.as_deref(), Some("bytes"));
    assert_eq!(no_range.bytes, bytes);

    let first_four = app
        .get_with_range(
            &format!("/files/{file_id}"),
            Some(&admin_cookie),
            Some("bytes=0-3"),
        )
        .await;
    assert_eq!(first_four.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        first_four.content_range.as_deref(),
        Some(format!("bytes 0-3/{total}").as_str())
    );
    assert_eq!(first_four.bytes, &bytes[0..4]);

    let last_two = app
        .get_with_range(
            &format!("/files/{file_id}"),
            Some(&admin_cookie),
            Some("bytes=-2"),
        )
        .await;
    assert_eq!(last_two.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        last_two.content_range.as_deref(),
        Some(format!("bytes {}-{}/{total}", total - 2, total - 1).as_str())
    );
    assert_eq!(last_two.bytes, &bytes[total - 2..]);

    let out_of_range = app
        .get_with_range(
            &format!("/files/{file_id}"),
            Some(&admin_cookie),
            Some(&format!("bytes={total}-{}", total + 10)),
        )
        .await;
    assert_eq!(out_of_range.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        out_of_range.content_range.as_deref(),
        Some(format!("bytes */{total}").as_str())
    );
}

#[tokio::test]
async fn the_viewer_renders_in_page_for_a_renderable_file_and_ignores_a_foreign_or_missing_one() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Open it in place").await;
    let other_task = a_task(&app, &admin_cookie, &column, "Not this one").await;

    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some(("spec.png", "image/png", &png)),
    )
    .await;
    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "spec.png");

    let page = app
        .get(
            &format!("/?task={task}&file={file_id}"),
            Some(&admin_cookie),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    // "viewer-body" marks the rendered overlay; the viewer's Escape resolver
    // that escape_closes registers (priority 90) also appears here on task
    // pages only, not every board page.
    assert!(
        html.contains("viewer-body"),
        "no viewer overlay in the page: {html}"
    );
    assert!(
        html.contains("__izlekEsc.register(90"),
        "no viewer escape resolver in the page: {html}"
    );
    assert!(
        html.contains(&format!("/files/{file_id}")),
        "viewer's <img> does not point at the file: {html}"
    );

    // A file id that exists but belongs to another task is refused the same
    // silent way a made-up one is: no overlay, the task modal alone.
    let wrong_task_page = app
        .get(
            &format!("/?task={other_task}&file={file_id}"),
            Some(&admin_cookie),
        )
        .await;
    assert!(!String::from_utf8_lossy(&wrong_task_page.bytes).contains("viewer-body"));

    let missing_page = app
        .get(&format!("/?task={task}&file=anything"), Some(&admin_cookie))
        .await;
    assert_eq!(missing_page.status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&missing_page.bytes).contains("viewer-body"));
}

/// A file is opened from a section, so closing it lands back on that section:
/// the close links carry the tab the viewer was opened over, not the panel's
/// default.
#[tokio::test]
async fn closing_a_file_returns_to_the_tab_it_was_opened_from() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Open it in place").await;
    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some(("spec.png", "image/png", &png)),
    )
    .await;
    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "spec.png");

    let page = app
        .get(
            &format!("/?task={task}&tab=files&file={file_id}"),
            Some(&admin_cookie),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("viewer-body"), "no viewer overlay: {html}");
    // The tab strip behind the overlay carries the same href, so the check
    // names the close control itself.
    assert!(
        html.contains(&format!(
            "class=\"viewer-close\" href=\"/?task={task}&amp;tab=files\""
        )),
        "viewer closes to the bare task, losing the section: {html}"
    );
}

#[tokio::test]
async fn a_file_past_the_workspace_limit_is_refused_before_it_is_kept() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Too big to keep").await;

    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "1"),
                ("photo_limit_mb", "2"),
                ("allowed_file_types", ""),
                ("mail_batch_minutes", "5"),
            ],
        )
        .await;
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        answer.location
    );

    let big = vec![0u8; 2 * 1024 * 1024];
    let answer = app
        .post_multipart(
            "/files",
            Some(&admin_cookie),
            &[("task_id", &task)],
            Some(("big.bin", "application/octet-stream", &big)),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some(format!("/?task={task}&tab=files&refusal=file-too-big&on=upload_file").as_str())
    );

    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    assert!(snapshot.body.contains("\"files\":[]"), "{}", snapshot.body);
}

#[tokio::test]
async fn a_file_type_off_the_list_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "No executables here").await;

    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "25"),
                ("photo_limit_mb", "2"),
                ("allowed_file_types", "png"),
                ("mail_batch_minutes", "5"),
            ],
        )
        .await;
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        answer.location
    );

    let answer = app
        .post_multipart(
            "/files",
            Some(&admin_cookie),
            &[("task_id", &task)],
            Some(("evil.exe", "application/octet-stream", b"MZ\x90\x00")),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some(format!("/?task={task}&tab=files&refusal=file-type&on=upload_file").as_str())
    );
}

#[tokio::test]
async fn an_empty_allowed_list_lets_anything_through() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Whatever shows up").await;

    let answer = app
        .post_multipart(
            "/files",
            Some(&admin_cookie),
            &[("task_id", &task)],
            Some((
                "anything.bin",
                "application/octet-stream",
                &[0x00, 0x01, 0x02],
            )),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some(format!("/?task={task}&tab=files").as_str())
    );
}

#[tokio::test]
async fn the_stored_type_is_what_the_bytes_are_not_what_the_upload_claimed() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Mislabeled on the way in").await;

    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let answer = app
        .post_multipart(
            "/files",
            Some(&admin_cookie),
            &[("task_id", &task)],
            Some(("liar.pdf", "application/pdf", &png)),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);

    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "liar.pdf");

    let download = app
        .get(&format!("/files/{file_id}"), Some(&admin_cookie))
        .await;
    assert_eq!(download.content_type.as_deref(), Some("image/png"));
}

#[tokio::test]
async fn a_file_name_that_is_a_path_is_kept_as_a_label() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Filename tries to escape").await;

    let answer = app
        .post_multipart(
            "/files",
            Some(&admin_cookie),
            &[("task_id", &task)],
            Some(("../../etc/passwd", "text/plain", b"root:x:0:0")),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);

    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    assert!(
        snapshot.body.contains("\"name\":\"passwd\""),
        "the stored name still has a path in it: {}",
        snapshot.body
    );
    let file_id = attachment_id_named(&snapshot.body, "passwd");

    let page = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        !html.contains("../../etc/passwd"),
        "the raw path leaked onto the chip: {html}"
    );

    let download = app
        .get(&format!("/files/{file_id}"), Some(&admin_cookie))
        .await;
    let disposition = download
        .disposition
        .expect("no content-disposition on the download");
    assert!(!disposition.contains('\r'), "{disposition}");
    assert!(!disposition.contains('\n'), "{disposition}");
    assert!(!disposition.contains('/'), "{disposition}");
}

/// A second workspace is a second database in this harness — there is no way
/// to build two workspaces sharing one store to prove sibling-workspace
/// isolation directly. What this proves instead: an attachment id that is
/// real, just not in *this* store, gets the same 404 an id from nowhere gets.
#[tokio::test]
async fn a_file_from_another_workspace_is_not_found() {
    let app_a = App::open().await;
    let admin_a = admin(&app_a).await;

    let app_b = App::open().await;
    let admin_b = admin(&app_b).await;
    let column_b = first_column(&app_b).await;
    let task_b = a_task(&app_b, &admin_b, &column_b, "Lives in the other workspace").await;
    let answer = app_b
        .post_multipart(
            "/files",
            Some(&admin_b),
            &[("task_id", &task_b)],
            Some(("theirs.png", "image/png", &[0x89, 0x50, 0x4E, 0x47])),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let snapshot = app_b
        .post("/api/fetch_task", Some(&admin_b), &[("task_id", &task_b)])
        .await;
    let file_id = attachment_id_named(&snapshot.body, "theirs.png");

    let answer = app_a
        .get(&format!("/files/{file_id}"), Some(&admin_a))
        .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_upload_without_a_file_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Nothing was chosen").await;

    let answer = app
        .post_multipart("/files", Some(&admin_cookie), &[("task_id", &task)], None)
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some(format!("/?task={task}&tab=files&refusal=no-file&on=upload_file").as_str())
    );
}

#[tokio::test]
async fn only_the_uploader_or_an_admin_may_delete_a_file() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member_a = invited(&app, &admin_cookie, "asha@izlek.sh", "Asha", Role::Member).await;
    let member_b = invited(&app, &admin_cookie, "beau@izlek.sh", "Beau", Role::Member).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Two uploaders, one task").await;

    let answer = app
        .post_multipart(
            "/files",
            Some(&member_a),
            &[("task_id", &task)],
            Some(("mine.png", "image/png", &[0x89, 0x50, 0x4E, 0x47])),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let answer = app
        .post_multipart(
            "/files",
            Some(&member_b),
            &[("task_id", &task)],
            Some(("theirs.png", "image/png", &[0x89, 0x50, 0x4E, 0x47])),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);

    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_a = attachment_id_named(&snapshot.body, "mine.png");
    let file_b = attachment_id_named(&snapshot.body, "theirs.png");

    let answer = app
        .post("/api/delete_file", Some(&member_b), &[("file_id", &file_a)])
        .await;
    assert_eq!(answer.body, "\"Forbidden\"", "{}", answer.body);

    let answer = app
        .post("/api/delete_file", Some(&member_a), &[("file_id", &file_a)])
        .await;
    assert_eq!(
        answer.body, "null",
        "the uploader was refused: {}",
        answer.body
    );

    let answer = app
        .post(
            "/api/delete_file",
            Some(&admin_cookie),
            &[("file_id", &file_b)],
        )
        .await;
    assert_eq!(
        answer.body, "null",
        "the admin was refused: {}",
        answer.body
    );

    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    assert!(snapshot.body.contains("\"files\":[]"), "{}", snapshot.body);
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// The id of the assignable person on a task whose display name matches.
async fn person_id(app: &App, cookie: &str, task_id: &str, name: &str) -> String {
    let answer = app
        .post("/api/fetch_task", Some(cookie), &[("task_id", task_id)])
        .await;
    let needle = format!("\"display_name\":\"{name}\"");
    let before = answer
        .body
        .split_once(&needle)
        .map(|(head, _)| head)
        .unwrap_or_else(|| panic!("no such person in {}", answer.body));
    before
        .rsplit_once("\"id\":\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no id before the display name")
        .to_string()
}

/// Reads the admin's logs until the snapshot contains `needle`, since the
/// engine runs off the request in a spawned task. Bounded so a snapshot that
/// never arrives fails the test instead of hanging it.
async fn until_logs_contains(app: &App, admin: &str, needle: &str) -> String {
    for _ in 0..100 {
        let answer = app.post("/api/current_logs", Some(admin), &[]).await;
        if answer.body.contains(needle) {
            return answer.body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the logs never showed {needle:?}");
}

#[tokio::test]
async fn only_an_admin_may_read_the_logs() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let viewer = invited(&app, &admin_cookie, "pinar@izlek.sh", "Pinar", Role::Viewer).await;

    for who in [&member, &viewer] {
        let read = app.post("/api/current_logs", Some(who), &[]).await;
        assert!(read.body.contains("Forbidden"), "{}", read.body);
    }

    let out = app.post("/api/current_logs", None, &[]).await;
    assert!(out.body.contains("SignInFirst"), "{}", out.body);
}

#[tokio::test]
async fn an_admin_reads_the_logs() {
    let app = App::open_with_mail().await;
    let admin_cookie = admin(&app).await;
    let mate = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let columns = columns_of(&app).await;
    let task = a_task(&app, &admin_cookie, &columns[0], "Ship it").await;
    let mate_id = person_id(&app, &admin_cookie, &task, "Emre").await;

    let assigned = app
        .post(
            "/api/assign",
            Some(&admin_cookie),
            &[("task_id", &task), ("user_id", &mate_id)],
        )
        .await;
    assert_eq!(assigned.body, "null", "{}", assigned.body);
    assert!(rule_written(&app, &admin_cookie, &columns[1], "Task completed").await);

    // Emre is the only assignee and Emre moves the card himself: the audience
    // empties out to nobody, and the decision says so rather than owing a
    // mail that would only tell him what he just did.
    let moved = app
        .post(
            "/api/move_card",
            Some(&mate),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(moved.body, "");

    let snapshot = until_logs_contains(&app, &admin_cookie, "\"outcome\":\"nobody to mail\"").await;
    // The queue still carries Emre's invite mail — unrelated to this rule —
    // so the check is that the rule itself queued nothing, not an empty queue.
    assert!(
        !snapshot.contains("\"subject\":\"Task completed\""),
        "{}",
        snapshot
    );

    // The admin drops it back and moves it again: this time the mover is not
    // the assignee, so the rule owes Emre a mail. With no sender configured
    // the send is not a failure — it waits in the queue.
    let back = app
        .post(
            "/api/move_card",
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[1]),
                ("to_column_id", &columns[0]),
            ],
        )
        .await;
    assert_eq!(back.body, "");
    let forward = app
        .post(
            "/api/move_card",
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(forward.body, "");

    // No sender means the send is held, not sent — the ledger stores that as
    // a failure with nothing spent, and the queue names the truth: held.
    let snapshot =
        until_logs_contains(&app, &admin_cookie, "\"recipient\":\"emre@izlek.sh\"").await;
    assert!(snapshot.contains("\"state\":\"held\""), "{}", snapshot);
    assert!(snapshot.contains("\"attempts\":0"), "{}", snapshot);
}

/// The `"at"` field of the activity row whose `"title"` matches, read out of
/// a `/api/current_logs` body the way `person_id` reads an id.
fn moment_for(body: &str, title: &str) -> String {
    let needle = format!("\"title\":\"{title}\"");
    let before = body
        .split_once(&needle)
        .map(|(head, _)| head)
        .unwrap_or_else(|| panic!("no such title in {body}"));
    before
        .rsplit_once("\"at\":\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no at before the title")
        .to_string()
}

/// The hour out of a `moment_label`-shaped stamp like `"Aug 19 11:04"`.
fn hour_of(moment: &str) -> u32 {
    moment
        .rsplit(' ')
        .next()
        .and_then(|hm| hm.split(':').next())
        .and_then(|h| h.parse().ok())
        .unwrap_or_else(|| panic!("not a moment: {moment}"))
}

#[tokio::test]
async fn a_stamp_shifts_with_the_viewers_stored_timezone() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let columns = columns_of(&app).await;
    let _task = a_task(&app, &admin_cookie, &columns[0], "Ship it").await;

    let utc = until_logs_contains(&app, &admin_cookie, "\"title\":\"Ship it\"").await;
    let utc_at = moment_for(&utc, "Ship it");

    let saved = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("timezone", "UTC+03:00")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let shifted = app
        .post("/api/current_logs", Some(&admin_cookie), &[])
        .await;
    let shifted_at = moment_for(&shifted.body, "Ship it");

    assert_ne!(utc_at, shifted_at, "utc={utc_at} shifted={shifted_at}");
    assert_eq!(
        hour_of(&shifted_at),
        (hour_of(&utc_at) + 3) % 24,
        "utc={utc_at} shifted={shifted_at}"
    );
}

/// The first `activity-stamp` span's text out of a task modal's HTML — the
/// same shape `moment_for` reads out of a `/api/current_logs` body.
fn activity_stamp_of(html: &str) -> &str {
    let (_, rest) = html
        .split_once(r#"class="activity-stamp">"#)
        .unwrap_or_else(|| panic!("no activity stamp in {html}"));
    rest.split_once('<')
        .map(|(stamp, _)| stamp)
        .expect("unterminated activity stamp")
}

#[tokio::test]
async fn a_task_modal_stamp_shifts_with_the_viewers_stored_timezone() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    // Activity lives behind its own tab now, so the stamp is asked for there.
    let page = app
        .get(&format!("/?task={task}&tab=activity"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    let utc_at = activity_stamp_of(&html).to_string();

    let saved = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("timezone", "UTC+03:00")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let shifted_page = app
        .get(&format!("/?task={task}&tab=activity"), Some(&admin_cookie))
        .await;
    let shifted_html = String::from_utf8_lossy(&shifted_page.bytes);
    let shifted_at = activity_stamp_of(&shifted_html).to_string();

    assert_ne!(utc_at, shifted_at, "utc={utc_at} shifted={shifted_at}");
    assert_eq!(
        hour_of(&shifted_at),
        (hour_of(&utc_at) + 3) % 24,
        "utc={utc_at} shifted={shifted_at}"
    );
}

#[tokio::test]
async fn an_unlisted_timezone_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[
                ("display_name", "Ada Lovelace"),
                ("timezone", "Mars/Olympus_Mons"),
            ],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-zone&on=save_profile"),
        "{location}"
    );
}

#[tokio::test]
async fn the_dark_theme_is_saved_and_marks_the_page() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let saved = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("theme", "dark")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let page = app.get("/settings", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains(r#"data-theme="dark""#), "{html}");
}

#[tokio::test]
async fn turkish_is_saved_and_the_board_renders_in_turkish() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let saved = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("language", "tr")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let page = app.get("/", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains(r#"<html lang="tr""#), "{html}");
    assert!(html.contains("Ayarlar"), "{html}");

    let settings_page = app.get("/settings", Some(&admin_cookie)).await;
    let settings_html = String::from_utf8_lossy(&settings_page.bytes);
    assert!(settings_html.contains("Profilin"), "{settings_html}");

    let rules_page = app.get("/rules", Some(&admin_cookie)).await;
    let rules_html = String::from_utf8_lossy(&rules_page.bytes);
    assert!(rules_html.contains("Posta kuralları"), "{rules_html}");
}

#[tokio::test]
async fn an_unlisted_language_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let saved = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("language", "fr")],
        )
        .await;
    assert_eq!(
        saved.location.as_deref(),
        Some("/settings?refusal=bad-language&on=save_profile&section=profile")
    );
}

#[tokio::test]
async fn an_unlisted_theme_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("theme", "neon")],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-theme&on=save_profile"),
        "{location}"
    );
}

#[tokio::test]
async fn the_ledger_ui_is_saved_and_marks_the_page() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let saved = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("ui", "ledger")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let page = app.get("/settings", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains(r#"data-ui="ledger""#), "{html}");
}

#[tokio::test]
async fn an_unlisted_ui_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("ui", "neon")],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-ui&on=save_profile"),
        "{location}"
    );

    let page = app.get("/settings", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains(r#"data-ui="instrument""#), "{html}");
}

/// Creating a task files its own Created activity, but the column it lands
/// in also fires a `Transition` the way a drop does — a `StatusBecomes` rule
/// armed on that column must owe mail on creation, not only on a later move.
#[tokio::test]
async fn creating_a_task_into_a_ruled_column_owes_mail() {
    let app = App::open_with_mail().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "deniz@izlek.sh", "Deniz", Role::Member).await;
    let column = first_column(&app).await;

    let written = app
        .post(
            "/api/create_rule",
            Some(&admin_cookie),
            &[
                ("trigger", "status"),
                ("column_id", &column),
                ("subject", "New card"),
                ("audience", "board"),
            ],
        )
        .await;
    assert_eq!(written.body, "null", "{}", written.body);
    let rule = only_rule(&app, &admin_cookie).await;

    // Deniz creates the card, so the board audience (which excludes the
    // actor) resolves to the admin — the only other person on the board.
    let created = app
        .post(
            "/api/create_task",
            Some(&member),
            &[("title", "Ship it"), ("column_id", &column)],
        )
        .await;
    assert_eq!(created.body, "", "the task was refused: {}", created.body);

    let send = until_rule_send_to(&app, &rule, "ada@izlek.sh", 0).await;
    assert_eq!(send.recipient, "ada@izlek.sh");
}

#[tokio::test]
async fn the_topbar_nav_marks_the_active_page() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    for (path, active) in [
        ("/", "/"),
        ("/rules", "/rules"),
        ("/logs", "/logs"),
        ("/settings", "/settings"),
    ] {
        let page = app.get(path, Some(&admin_cookie)).await;
        assert_eq!(page.status, StatusCode::OK);
        let html = String::from_utf8_lossy(&page.bytes);
        for nav in ["/", "/rules", "/logs", "/settings"] {
            let expected = if nav == active {
                format!(r#"class="topbar-nav topbar-nav-on" href="{nav}""#)
            } else {
                format!(r#"class="topbar-nav" href="{nav}""#)
            };
            assert!(
                html.contains(&expected),
                "page {path} lacks `{expected}`: {html}"
            );
        }
    }

    // The settings rail keeps the panels' admin gating: a member sees only
    // the Profile link, and an admin-only section in the query still renders
    // Profile rather than the panel it asked for.
    let member = invited(&app, &admin_cookie, "deniz@izlek.sh", "Deniz", Role::Member).await;
    let page = app.get("/settings", Some(&member)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(r#"href="/settings?section=profile""#),
        "{html}"
    );
    assert!(
        !html.contains("href=\"/settings?section=limits\""),
        "{html}"
    );
    assert!(
        !html.contains("href=\"/settings?section=members\""),
        "{html}"
    );
    assert!(html.contains(r#"id="profile""#), "{html}");
    assert!(!html.contains(r#"id="limits""#), "{html}");

    let page = app.get("/settings?section=limits", Some(&member)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains(r#"id="profile""#), "{html}");
    assert!(!html.contains(r#"id="limits""#), "{html}");
}

/// A wrong current password lands back on the rail section it was posted
/// from with a refusal; a right one changes nothing about the query but the
/// refusal — `carry_refusal_on_redirect` never writes a `saved=` for a call
/// nobody asked to see a note about.
#[tokio::test]
async fn a_password_change_carries_its_own_refusal_and_never_a_saved_note() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let wrong = app
        .post_without_script(
            "/api/change_password",
            Some(&admin_cookie),
            "http://izlek.test/settings?section=profile",
            &[
                ("current", "not the password"),
                ("new", "a whole new passphrase"),
            ],
        )
        .await;
    assert!(
        wrong.location.as_deref().is_some_and(|location| {
            location.contains("refusal=")
                && location.contains("on=change_password")
                && location.contains("section=profile")
        }),
        "{:?}",
        wrong.location
    );

    let right = app
        .post_without_script(
            "/api/change_password",
            Some(&admin_cookie),
            "http://izlek.test/settings?section=profile",
            &[
                ("current", "correct horse battery staple"),
                ("new", "a whole new passphrase"),
            ],
        )
        .await;
    assert_eq!(right.status, StatusCode::SEE_OTHER);
    assert!(
        right
            .location
            .as_deref()
            .is_some_and(|location| !location.contains("saved=")),
        "{:?}",
        right.location
    );
}

/// Polls the store until a `Rule` send for `rule_id` addressed to `recipient`
/// exists beyond the `already` count — the engine runs off the request in a
/// spawned task, so the row is not there yet when the triggering call
/// returns. Bounded so a send that never arrives fails the test instead of
/// hanging it.
async fn until_rule_send_to(
    app: &App,
    rule_id: &str,
    recipient: &str,
    already: usize,
) -> izlek_core::store::MailSend {
    for _ in 0..500 {
        let matching: Vec<_> = app
            .store
            .mail_queue(50, izlek_core::store::FeedPage::Newest)
            .await
            .unwrap()
            .into_iter()
            .filter(|send| {
                send.kind == SendKind::Rule
                    && send.rule_id.as_deref() == Some(rule_id)
                    && send.recipient == recipient
            })
            .collect();
        if matching.len() > already {
            return matching.into_iter().next().unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no rule send to {recipient} for rule {rule_id} ever queued");
}

/// One rule rides two different events across its lifetime, and a rewrite in
/// place keeps riding the second without becoming a new rule: a comment mails
/// the task's creator, not the commenter, and after the rule is rewritten to
/// fire on a rename instead, a rename mails the assignee, not the renamer.
#[tokio::test]
async fn a_rule_rides_every_event_and_can_be_rewritten() {
    let app = App::open_with_mail().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "deniz@izlek.sh", "Deniz", Role::Member).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship the picker").await;

    let created = app
        .post(
            "/api/create_rule",
            Some(&admin_cookie),
            &[
                ("trigger", "commented"),
                ("column_id", ""),
                ("subject", "Someone commented"),
                ("audience", "creator"),
            ],
        )
        .await;
    assert_eq!(created.body, "null", "{}", created.body);
    let rule = only_rule(&app, &admin_cookie).await;

    // A second member comments; the task's creator is the admin, not them.
    let commented = app
        .post(
            "/api/post_comment",
            Some(&member),
            &[("task_id", &task), ("body", "Looks good")],
        )
        .await;
    assert_eq!(commented.body, "null", "{}", commented.body);

    let send = until_rule_send_to(&app, &rule, "ada@izlek.sh", 0).await;
    assert_eq!(send.recipient, "ada@izlek.sh");
    assert!(
        app.store
            .mail_queue(50, izlek_core::store::FeedPage::Newest)
            .await
            .unwrap()
            .iter()
            .all(|send| {
                !(send.kind == SendKind::Rule
                    && send.rule_id.as_deref() == Some(rule.as_str())
                    && send.recipient == "deniz@izlek.sh")
            }),
        "the commenter was mailed instead of the creator"
    );
    let decisions = app
        .store
        .recent_mail_decisions(50, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    assert!(
        decisions.iter().any(|decision| decision.rule_id == rule
            && matches!(decision.outcome, izlek_core::store::MailOutcome::Owed)),
        "the decisions ledger has no matched decision for the rule"
    );

    // The admin is put on the task so the rewritten rule's assignees audience
    // has someone to address once it fires on a rename.
    let admin_id = person_id(&app, &admin_cookie, &task, "Ada Lovelace").await;
    let assigned = app
        .post(
            "/api/assign",
            Some(&admin_cookie),
            &[("task_id", &task), ("user_id", &admin_id)],
        )
        .await;
    assert_eq!(assigned.body, "null", "{}", assigned.body);

    // The rule is rewritten in place: same id, new trigger, subject and
    // audience.
    let updated = app
        .post(
            "/api/update_rule",
            Some(&admin_cookie),
            &[
                ("rule_id", &rule),
                ("trigger", "retitled"),
                ("column_id", ""),
                ("subject", "Renamed"),
                ("audience", "assignees"),
            ],
        )
        .await;
    assert_eq!(
        updated.body, "null",
        "the rewrite was refused: {}",
        updated.body
    );
    assert_eq!(
        only_rule(&app, &admin_cookie).await,
        rule,
        "a new rule was made instead of the old one rewritten"
    );

    let seen = app
        .post("/api/current_rules", Some(&admin_cookie), &[])
        .await;
    assert!(
        seen.body.contains(&format!("\"id\":\"{rule}\"")),
        "{}",
        seen.body
    );
    assert!(seen.body.contains("\"enabled\":true"), "{}", seen.body);
    assert!(
        seen.body.contains("\"trigger_kind\":\"retitled\""),
        "{}",
        seen.body
    );
    assert!(
        seen.body.contains("\"subject\":\"Renamed\""),
        "{}",
        seen.body
    );

    // The second member renames the task; the rule now fires on retitle and
    // addresses the assignee — the admin — not the member who renamed it.
    let already = app
        .store
        .mail_queue(50, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap()
        .iter()
        .filter(|send| {
            send.kind == SendKind::Rule
                && send.rule_id.as_deref() == Some(rule.as_str())
                && send.recipient == "ada@izlek.sh"
        })
        .count();
    let renamed = app
        .post(
            "/api/save_task",
            Some(&member),
            &[("task_id", &task), ("title", "Ship the redesigned picker")],
        )
        .await;
    assert_eq!(renamed.body, "null", "{}", renamed.body);

    until_rule_send_to(&app, &rule, "ada@izlek.sh", already).await;
    assert!(
        app.store
            .mail_queue(50, izlek_core::store::FeedPage::Newest)
            .await
            .unwrap()
            .iter()
            .all(|send| {
                !(send.kind == SendKind::Rule
                    && send.rule_id.as_deref() == Some(rule.as_str())
                    && send.recipient == "deniz@izlek.sh")
            }),
        "the renamer was mailed instead of being excluded as the actor"
    );

    let logs = until_logs_contains(&app, &admin_cookie, "\"subject\":\"Renamed\"").await;
    assert!(logs.contains("\"recipient\":\"ada@izlek.sh\""), "{}", logs);
}

// `?task=X&new=1` together used to render both modals at once — two
// document-level datepicker listeners double-stepping the month nav.
#[tokio::test]
async fn task_and_new_together_render_only_the_task_modal() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin, &column, "Only one modal at a time").await;

    let raw = app.get(&format!("/?task={task}&new=1"), Some(&admin)).await;
    let html = String::from_utf8_lossy(&raw.bytes);
    assert_eq!(html.matches("class=\"modal-scrim\"").count(), 1, "{html}");
    assert!(
        !html.contains("modal-new-task"),
        "the new-task modal rendered too: {html}"
    );
}

#[tokio::test]
async fn the_new_task_modal_opens_from_the_board_and_creates_into_the_chosen_column() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app).await;
    let column = columns.last().expect("no columns on a fresh board").clone();

    let raw = app.get("/?new=1", Some(&admin)).await;
    let html = String::from_utf8_lossy(&raw.bytes);
    assert!(
        html.contains("modal-new-task"),
        "the new-task modal did not render: {html}"
    );
    assert!(
        html.contains(&format!("value=\"{column}\"")),
        "the column picker is missing a board column: {html}"
    );

    let answer = app
        .post(
            "/api/create_task",
            Some(&admin),
            &[("title", "Ship the new-task modal"), ("column_id", &column)],
        )
        .await;
    assert_eq!(answer.body, "", "the create was refused: {}", answer.body);

    // A browser without script posts from `/?new=1`; a success has to land on
    // the board, not reopen the (now stale) new-task modal.
    let no_script = app
        .post_without_script(
            "/api/create_task",
            Some(&admin),
            "http://izlek.test/?new=1",
            &[("title", "Ship it, no script"), ("column_id", &column)],
        )
        .await;
    assert_eq!(
        no_script.location.as_deref(),
        Some("/"),
        "{:?}",
        no_script.location
    );

    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id)
        .await
        .unwrap()
        .unwrap();
    let card = board
        .columns
        .iter()
        .find(|c| c.column.id == column)
        .and_then(|c| {
            c.cards
                .iter()
                .find(|card| card.title == "Ship the new-task modal")
        })
        .expect("the created task did not land in the chosen column");
    assert_eq!(card.title, "Ship the new-task modal");
}
/// A 1×1 transparent PNG: 67 real bytes, header to checksum, so it sniffs as
/// `image/png` and survives a byte-for-byte round trip.
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// The id of the account behind an email address, read straight off the store:
/// fixture setup, not the behavior under test.
async fn user_id(app: &App, email: &str) -> String {
    app.store
        .user_by_email(&app.workspace_id().await, email)
        .await
        .unwrap()
        .expect("no such user")
        .id
}

/// A profile photo uploads, and `GET /photo/{id}` serves those exact bytes
/// back as `image/png`.
#[tokio::test]
async fn a_profile_photo_round_trips_back_as_the_same_bytes() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let admin_id = user_id(&app, "ada@izlek.sh").await;

    let answer = app
        .post_multipart(
            "/api/profile_photo",
            Some(&admin),
            &[],
            Some(("me.png", "image/png", &PNG)),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/settings?saved=profile_photo&section=profile")
    );

    let photo = app.get(&format!("/photo/{admin_id}"), Some(&admin)).await;
    assert_eq!(photo.status, StatusCode::OK);
    assert_eq!(photo.content_type.as_deref(), Some("image/png"));
    assert_eq!(photo.bytes, PNG);
}

/// Text bytes wearing a `.png` name do not sniff as an image: refused on the
/// redirect, and nothing stored to serve.
#[tokio::test]
async fn text_uploaded_as_a_photo_is_refused_and_stores_nothing() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let admin_id = user_id(&app, "ada@izlek.sh").await;

    let answer = app
        .post_multipart(
            "/api/profile_photo",
            Some(&admin),
            &[],
            Some(("note.png", "image/png", b"just some words")),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/settings?refusal=not-an-image&on=profile_photo&section=profile")
    );

    let photo = app.get(&format!("/photo/{admin_id}"), Some(&admin)).await;
    assert_eq!(photo.status, StatusCode::NOT_FOUND);
}

/// The workspace's photo cap is enforced on the way in, and an over-cap upload
/// stores nothing.
#[tokio::test]
async fn a_photo_over_the_workspace_limit_is_refused_and_stores_nothing() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let admin_id = user_id(&app, "ada@izlek.sh").await;
    // The transport caps request bodies at 2 MiB (topcoat's default body
    // limit), exactly a fresh workspace's photo limit, so against the shipped
    // defaults the handler's own check can never fire. The admin lowers the
    // limit first, the way the /files oversize test does, and the upload is
    // sized just past that.
    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin),
            &[
                ("attachment_limit_mb", "25"),
                ("photo_limit_mb", "1"),
                ("allowed_file_types", ""),
                ("mail_batch_minutes", "5"),
            ],
        )
        .await;
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        answer.location
    );

    let limit = app
        .store
        .workspace()
        .await
        .unwrap()
        .expect("no workspace after claiming")
        .photo_limit_bytes as usize;
    let big = vec![0u8; limit + 1];

    let answer = app
        .post_multipart(
            "/api/profile_photo",
            Some(&admin),
            &[],
            Some(("big.png", "image/png", &big)),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/settings?refusal=file-too-big&on=profile_photo&section=profile")
    );

    let photo = app.get(&format!("/photo/{admin_id}"), Some(&admin)).await;
    assert_eq!(photo.status, StatusCode::NOT_FOUND);
}

/// A photo belongs to its person, not to the uploader: everyone in the same
/// workspace can see it.
#[tokio::test]
async fn a_photo_is_visible_to_other_workspace_members() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let admin_id = user_id(&app, "ada@izlek.sh").await;
    let member = invited(&app, &admin, "deniz@izlek.sh", "Deniz", Role::Member).await;

    let answer = app
        .post_multipart(
            "/api/profile_photo",
            Some(&admin),
            &[],
            Some(("me.png", "image/png", &PNG)),
        )
        .await;
    assert_eq!(
        answer.location.as_deref(),
        Some("/settings?saved=profile_photo&section=profile")
    );

    let photo = app.get(&format!("/photo/{admin_id}"), Some(&member)).await;
    assert_eq!(photo.status, StatusCode::OK);
    assert_eq!(photo.content_type.as_deref(), Some("image/png"));
    assert_eq!(photo.bytes, PNG);
}

/// No such person, no photo: a stranger id is the not-found a person without a
/// photo would see. A known photo carries an `ETag`, and a matching
/// `If-None-Match` gets the empty 304.
#[tokio::test]
async fn an_unknown_photo_id_is_not_found_and_the_photo_revalidates_by_etag() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let admin_id = user_id(&app, "ada@izlek.sh").await;

    let answer = app
        .post_multipart(
            "/api/profile_photo",
            Some(&admin),
            &[],
            Some(("me.png", "image/png", &PNG)),
        )
        .await;
    assert_eq!(
        answer.location.as_deref(),
        Some("/settings?saved=profile_photo&section=profile")
    );

    let stranger = app.get("/photo/does-not-exist", Some(&admin)).await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);

    let photo = app.get(&format!("/photo/{admin_id}"), Some(&admin)).await;
    assert_eq!(photo.status, StatusCode::OK);
    let etag = photo.etag.clone().expect("no ETag on the served photo");

    let cached = app
        .get_with_if_none_match(&format!("/photo/{admin_id}"), Some(&admin), &etag)
        .await;
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);
    assert!(cached.bytes.is_empty());
}

/// The whole point of a versioned photo URL: replacing the photo must move
/// the stamp the avatar renders, or the browser keeps serving the bytes it
/// cached under the old URL and the replacement never shows.
#[tokio::test]
async fn a_replaced_photo_moves_the_avatar_url_stamp() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let admin_id = user_id(&app, "ada@izlek.sh").await;

    app.post_multipart(
        "/api/profile_photo",
        Some(&admin),
        &[],
        Some(("me.png", "image/png", &PNG)),
    )
    .await;
    let page = app.get("/settings", Some(&admin)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    let first = avatar_stamp(&html, &admin_id).expect("avatar photo URL on the settings page");

    let mut replaced = PNG;
    replaced[11] ^= 0xff;
    app.post_multipart(
        "/api/profile_photo",
        Some(&admin),
        &[],
        Some(("me.png", "image/png", &replaced)),
    )
    .await;
    let page = app.get("/settings", Some(&admin)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    let second = avatar_stamp(&html, &admin_id).expect("avatar photo URL after replace");

    assert!(second > first, "stamp must move: {first} -> {second}");
}

/// The `?v=` on an avatar's `/photo/{id}` src, parsed out of a rendered page.
fn avatar_stamp(html: &str, user_id: &str) -> Option<i64> {
    let marker = format!("/photo/{user_id}?v=");
    let rest = &html[html.find(&marker)? + marker.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The nav shows a page only to a role that can act on it: an admin's board
/// carries all four links, a member's carries neither Rules nor Logs.
#[tokio::test]
async fn the_nav_hides_rules_and_logs_from_a_member() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "deniz@izlek.sh", "Deniz", Role::Member).await;

    let admin_board = app.get("/", Some(&admin)).await;
    let admin_html = String::from_utf8_lossy(&admin_board.bytes).into_owned();
    assert!(admin_html.contains("href=\"/rules\""));
    assert!(admin_html.contains("href=\"/logs\""));

    let member_board = app.get("/", Some(&member)).await;
    let member_html = String::from_utf8_lossy(&member_board.bytes).into_owned();
    assert!(member_html.contains("href=\"/settings\""));
    assert!(!member_html.contains("href=\"/rules\""));
    assert!(!member_html.contains("href=\"/logs\""));
}

/// Account events ride the same feed as task events: the admin's own
/// sign-in and an invite show up beside a created task, each on its actor.
#[tokio::test]
async fn account_events_ride_the_activity_feed() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let columns = columns_of(&app).await;
    a_task(&app, &admin_cookie, &columns[0], "Ship it").await;

    let fresh = app
        .post(
            "/api/sign_in",
            None,
            &[
                ("email", "ada@izlek.sh"),
                ("password", "correct horse battery staple"),
            ],
        )
        .await;
    let admin_cookie = fresh.session.expect("signing in set no session cookie");

    let body = until_logs_contains(&app, &admin_cookie, "invited emre@izlek.sh").await;
    assert!(body.contains("signed in"), "{body}");
    assert!(body.contains("claimed the workspace"), "{body}");
    assert!(body.contains("\"sentence\":\"joined\""), "{body}");
    assert!(body.contains("Ship it"), "{body}");
}

/// The href the page itself hands back for a link, so the test walks the
/// same road a reader would rather than predicting a query string.
fn extract_href<'h>(html: &'h str, contains: &str) -> &'h str {
    let start = html
        .match_indices("href=\"")
        .map(|(i, _)| i + "href=\"".len())
        .find(|&i| html[i..].starts_with(contains))
        .unwrap_or_else(|| panic!("no href containing {contains:?}: {html}"));
    let end = html[start..].find('"').unwrap() + start;
    &html[start..end]
}

/// The activity panel truncates at 50 rows. Rows shift under an OFFSET
/// reader turning the page; a keyset cursor cannot skip or duplicate one
/// because it names the row itself, not a position that can move. A page
/// turn here follows the actual `Older`/`Newer` href out of the body —
/// never predicts a query string — and a member GET still finds the door
/// shut.
#[tokio::test]
async fn the_logs_page_pages_past_the_truncation() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "deniz@izlek.sh", "Deniz", Role::Member).await;

    let t0 = time::OffsetDateTime::now_utc();
    for i in 0..60 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("row {i}"),
                t0 + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }

    let first = app.get("/logs?section=activity", Some(&admin_cookie)).await;
    let first_html = String::from_utf8_lossy(&first.bytes).into_owned();
    assert!(first_html.contains("row 59"), "{first_html}");
    // Row 10 is the 50th newest — the boundary row this page ends on.
    assert!(first_html.contains("row 10<"), "{first_html}");
    assert!(!first_html.contains("row 9<"), "{first_html}");
    assert!(
        !first_html.contains("\">Newer</a>"),
        "a Newer link on the newest page: {first_html}"
    );
    let older_href =
        extract_href(&first_html, "/logs?section=activity&amp;before=").replace("&amp;", "&");

    let second = app.get(&older_href, Some(&admin_cookie)).await;
    let second_html = String::from_utf8_lossy(&second.bytes).into_owned();
    // Row 10 shown once, on the first page — not skipped, not repeated here.
    assert!(!second_html.contains("row 10<"), "{second_html}");
    assert!(second_html.contains("row 9<"), "{second_html}");
    assert!(second_html.contains("row 0<"), "{second_html}");
    let newer_href =
        extract_href(&second_html, "/logs?section=activity&amp;after=").replace("&amp;", "&");
    // No Older link left: the second page is the whole remainder.
    assert!(
        !second_html.contains("\">Older</a>"),
        "an Older link past the last row: {second_html}"
    );

    let back = app.get(&newer_href, Some(&admin_cookie)).await;
    let back_html = String::from_utf8_lossy(&back.bytes).into_owned();
    assert!(back_html.contains("row 59"), "{back_html}");

    let refused = app.get("/logs", Some(&member)).await;
    let refused_html = String::from_utf8_lossy(&refused.bytes);
    assert!(refused_html.contains("Not permitted."), "{refused_html}");

    // A cursor that does not parse falls back to the newest page rather
    // than erroring.
    let garbage = app
        .get("/logs?section=activity&before=zzz", Some(&admin_cookie))
        .await;
    assert_eq!(garbage.status, 200);
    let garbage_html = String::from_utf8_lossy(&garbage.bytes);
    assert!(garbage_html.contains("row 59"), "{garbage_html}");
}

/// The activity filter row narrows the feed by actor, kind, task and day,
/// and the pager foot's position note counts against the filtered total —
/// not the whole feed.
#[tokio::test]
async fn the_activity_filter_narrows_and_the_position_note_counts_the_match() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let admin_id = user_id(&app, "ada@izlek.sh").await;
    let member = invited(&app, &admin_cookie, "deniz@izlek.sh", "Deniz", Role::Member).await;
    let member_id = user_id(&app, "deniz@izlek.sh").await;

    let t0 = time::OffsetDateTime::now_utc();
    for i in 0..60 {
        let actor = if i % 2 == 0 {
            Some(admin_id.as_str())
        } else {
            Some(member_id.as_str())
        };
        app.store
            .record_event(
                actor,
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("row {i}"),
                t0 + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }

    let all = app
        .get("/logs?section=activity&kind=row", Some(&admin_cookie))
        .await;
    let all_html = String::from_utf8_lossy(&all.bytes).into_owned();
    assert!(all_html.contains("1\u{2013}"), "{all_html}");
    assert!(all_html.contains("/ 60</span>"), "{all_html}");

    let by_actor = app
        .get(
            &format!("/logs?section=activity&kind=row&actor={member_id}"),
            Some(&admin_cookie),
        )
        .await;
    let by_actor_html = String::from_utf8_lossy(&by_actor.bytes).into_owned();
    assert!(by_actor_html.contains("/ 30</span>"), "{by_actor_html}");

    let oldest = app
        .get(
            "/logs?section=activity&kind=row&dir=oldest",
            Some(&admin_cookie),
        )
        .await;
    let oldest_html = String::from_utf8_lossy(&oldest.bytes).into_owned();
    assert!(oldest_html.contains("row 0<"), "{oldest_html}");

    let garbage = app
        .get(
            "/logs?section=activity&actor=zz&kind=zz&on=zz",
            Some(&admin_cookie),
        )
        .await;
    assert_eq!(garbage.status, 200);

    let refused = app
        .get("/logs?section=activity&actor=x", Some(&member))
        .await;
    let refused_html = String::from_utf8_lossy(&refused.bytes);
    assert!(refused_html.contains("Not permitted."), "{refused_html}");
}

/// `kind=` alone narrows the feed to one activity kind, leaving the other
/// kind's rows out of the body entirely.
#[tokio::test]
async fn the_kind_filter_shows_only_that_kinds_rows() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let t0 = time::OffsetDateTime::now_utc();
    for i in 0..5 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Created,
                "",
                t0 + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }
    for i in 0..3 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Commented,
                "",
                t0 + time::Duration::seconds(100 + i),
            )
            .await
            .unwrap();
    }

    let narrowed = app
        .get("/logs?section=activity&kind=created", Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&narrowed.bytes).into_owned();
    assert!(html.contains("created this task"), "{html}");
    assert!(!html.contains("log-line\">commented<"), "{html}");
    assert!(html.contains("/ 5</span>"), "{html}");
}

/// `task=` narrows the feed to one task's rows, by its key — not any other
/// task's, even one created in the same run.
#[tokio::test]
async fn the_task_filter_shows_only_that_tasks_rows() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let alpha = a_task(&app, &admin_cookie, &column, "Task Alpha").await;
    let _beta = a_task(&app, &admin_cookie, &column, "Task Beta").await;
    let alpha_key = app
        .store
        .task(&alpha)
        .await
        .unwrap()
        .expect("alpha task gone")
        .row
        .task_key;

    let narrowed = app
        .get(
            &format!("/logs?section=activity&task={alpha_key}"),
            Some(&admin_cookie),
        )
        .await;
    let html = String::from_utf8_lossy(&narrowed.bytes).into_owned();
    assert!(html.contains("log-title\">Task Alpha<"), "{html}");
    assert!(!html.contains("log-title\">Task Beta<"), "{html}");
}

/// The task filter is a select listing the workspace's tasks, keyed by
/// task_key — not a bare text input.
#[tokio::test]
async fn the_task_filter_lists_the_workspaces_tasks() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let alpha = a_task(&app, &admin_cookie, &column, "Task Alpha").await;
    let alpha_key = app
        .store
        .task(&alpha)
        .await
        .unwrap()
        .expect("alpha task gone")
        .row
        .task_key;

    let page = app.get("/logs?section=activity", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes).into_owned();
    assert!(
        html.contains(&format!("<option value=\"{alpha_key}\"")),
        "{html}"
    );
}

/// A row for a task-attached event carries the task's key alongside its
/// title.
#[tokio::test]
async fn the_activity_row_shows_the_task_key() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let alpha = a_task(&app, &admin_cookie, &column, "Task Alpha").await;
    let alpha_key = app
        .store
        .task(&alpha)
        .await
        .unwrap()
        .expect("alpha task gone")
        .row
        .task_key;

    let page = app.get("/logs?section=activity", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes).into_owned();
    assert!(
        html.contains(&format!("log-key\">({alpha_key})<")),
        "{html}"
    );
}

/// `on=YYYY-MM-DD` keeps only the rows recorded that day, in the admin's own
/// timezone (UTC by default in this fixture).
#[tokio::test]
async fn the_day_filter_keeps_only_that_days_rows() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let today = time::OffsetDateTime::now_utc();
    let two_days_ago = today - time::Duration::days(2);
    for i in 0..5 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("today-row {i}"),
                today + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }
    for i in 0..5 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("old-row {i}"),
                two_days_ago + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }
    let on = format!(
        "{:04}-{:02}-{:02}",
        today.year(),
        today.month() as u8,
        today.day()
    );

    let narrowed = app
        .get(
            &format!("/logs?section=activity&on={on}"),
            Some(&admin_cookie),
        )
        .await;
    let html = String::from_utf8_lossy(&narrowed.bytes).into_owned();
    assert!(html.contains("today-row"), "{html}");
    assert!(!html.contains("old-row"), "{html}");
}

/// `from=`/`to=` narrows the feed to an inclusive span of days, in the
/// admin's own timezone (UTC by default in this fixture): a row on the `to`
/// day itself is kept, not just the days strictly between.
#[tokio::test]
async fn the_range_filter_keeps_the_inclusive_span() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let base =
        time::OffsetDateTime::now_utc().replace_time(time::Time::from_hms(12, 0, 0).unwrap());
    for i in 0..5i64 {
        let day = base + time::Duration::days(i);
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("day{i}-row"),
                day,
            )
            .await
            .unwrap();
    }
    let ymd = |dt: time::OffsetDateTime| {
        format!("{:04}-{:02}-{:02}", dt.year(), dt.month() as u8, dt.day())
    };
    let from = ymd(base + time::Duration::days(1));
    let to = ymd(base + time::Duration::days(3));

    let ranged = app
        .get(
            &format!("/logs?section=activity&from={from}&to={to}"),
            Some(&admin_cookie),
        )
        .await;
    let html = String::from_utf8_lossy(&ranged.bytes).into_owned();
    assert!(!html.contains("day0-row"), "{html}");
    assert!(html.contains("day1-row"), "{html}");
    assert!(html.contains("day2-row"), "{html}");
    // The `to` day itself is included — the off-by-one that matters.
    assert!(html.contains("day3-row"), "{html}");
    assert!(!html.contains("day4-row"), "{html}");

    let from_only = app
        .get(
            &format!("/logs?section=activity&from={from}"),
            Some(&admin_cookie),
        )
        .await;
    let from_only_html = String::from_utf8_lossy(&from_only.bytes).into_owned();
    assert!(!from_only_html.contains("day0-row"), "{from_only_html}");
    assert!(from_only_html.contains("day1-row"), "{from_only_html}");
    assert!(from_only_html.contains("day4-row"), "{from_only_html}");

    let to_only = app
        .get(
            &format!("/logs?section=activity&to={to}"),
            Some(&admin_cookie),
        )
        .await;
    let to_only_html = String::from_utf8_lossy(&to_only.bytes).into_owned();
    assert!(to_only_html.contains("day0-row"), "{to_only_html}");
    assert!(to_only_html.contains("day3-row"), "{to_only_html}");
    assert!(!to_only_html.contains("day4-row"), "{to_only_html}");

    // A reversed range (from later than to) is swapped, not empty: same rows
    // as the correctly-ordered range, though the form itself keeps echoing
    // the raw (reversed) query values so it does not overrule what the user
    // typed.
    let reversed = app
        .get(
            &format!("/logs?section=activity&from={to}&to={from}"),
            Some(&admin_cookie),
        )
        .await;
    let reversed_html = String::from_utf8_lossy(&reversed.bytes).into_owned();
    for i in 1..=3 {
        assert!(
            reversed_html.contains(&format!("day{i}-row")),
            "{reversed_html}"
        );
    }
    assert!(!reversed_html.contains("day0-row"), "{reversed_html}");
    assert!(!reversed_html.contains("day4-row"), "{reversed_html}");

    // Garbage bounds narrow nothing rather than 500ing.
    let garbage = app
        .get("/logs?section=activity&from=zz&to=zz", Some(&admin_cookie))
        .await;
    assert_eq!(garbage.status, StatusCode::OK);
    let garbage_html = String::from_utf8_lossy(&garbage.bytes).into_owned();
    for i in 0..5 {
        assert!(
            garbage_html.contains(&format!("day{i}-row")),
            "{garbage_html}"
        );
    }
}

/// An Older href on a range-filtered page keeps carrying `from=`/`to=` so
/// paging never drops the range.
#[tokio::test]
async fn the_older_href_round_trips_the_range_filter() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let t0 = time::OffsetDateTime::now_utc();
    for i in 0..60 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("row {i}"),
                t0 + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }
    let from = format!("{:04}-{:02}-{:02}", t0.year(), t0.month() as u8, t0.day());
    let to = from.clone();

    let first = app
        .get(
            &format!("/logs?section=activity&from={from}&to={to}"),
            Some(&admin_cookie),
        )
        .await;
    let first_html = String::from_utf8_lossy(&first.bytes).into_owned();
    let older_href =
        extract_href(&first_html, "/logs?section=activity&amp;before=").replace("&amp;", "&");
    assert!(older_href.contains(&format!("from={from}")), "{older_href}");
    assert!(older_href.contains(&format!("to={to}")), "{older_href}");
}

/// The `izlek_rows_activity` cookie the page's own fit script sets caps the
/// page at that many rows, no more.
#[tokio::test]
async fn the_rows_cookie_caps_the_page_size() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let t0 = time::OffsetDateTime::now_utc();
    for i in 0..60 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("row {i}"),
                t0 + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }

    let capped = app
        .get_with_extra_cookie(
            "/logs?section=activity",
            Some(&admin_cookie),
            "izlek_rows_activity=15",
        )
        .await;
    let html = String::from_utf8_lossy(&capped.bytes).into_owned();
    assert_eq!(html.matches("class=\"log-row\"").count(), 15, "{html}");
    assert!(html.contains("\">Older</a>"), "{html}");
}

/// The Older href carries an active `kind=`/`dir=oldest` filter along with
/// its own cursor, and following it never turns up the other kind's rows.
#[tokio::test]
async fn the_older_href_round_trips_the_active_filter() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let t0 = time::OffsetDateTime::now_utc();
    for i in 0..60 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("target".to_string()),
                &format!("target {i}"),
                t0 + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }
    for i in 0..5 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("foreign".to_string()),
                &format!("foreign {i}"),
                t0 + time::Duration::seconds(1000 + i),
            )
            .await
            .unwrap();
    }

    let first = app
        .get(
            "/logs?section=activity&kind=target&dir=oldest",
            Some(&admin_cookie),
        )
        .await;
    let first_html = String::from_utf8_lossy(&first.bytes).into_owned();
    assert!(!first_html.contains("foreign"), "{first_html}");
    let older_href =
        extract_href(&first_html, "/logs?section=activity&amp;before=").replace("&amp;", "&");
    assert!(older_href.contains("kind=target"), "{older_href}");
    assert!(older_href.contains("dir=oldest"), "{older_href}");

    let second = app.get(&older_href, Some(&admin_cookie)).await;
    let second_html = String::from_utf8_lossy(&second.bytes).into_owned();
    assert!(!second_html.contains("foreign"), "{second_html}");
}

/// `/api/current_logs` keeps reading the unpaged newest page regardless of a
/// small `izlek_rows_activity` cookie — the JSON route never widened by the
/// page's own fit cookie.
#[tokio::test]
async fn current_logs_json_ignores_the_rows_cookie() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let t0 = time::OffsetDateTime::now_utc();
    for i in 0..60 {
        app.store
            .record_event(
                None,
                &izlek_core::detail::ActivityKind::Other("row".to_string()),
                &format!("row {i}"),
                t0 + time::Duration::seconds(i),
            )
            .await
            .unwrap();
    }

    let answer = app
        .post_with_extra_cookie(
            "/api/current_logs",
            Some(&admin_cookie),
            &[],
            Some("izlek_rows_activity=5"),
        )
        .await;
    assert!(answer.body.contains("\"queue\""), "{}", answer.body);
    assert!(answer.body.contains("\"decisions\""), "{}", answer.body);
    assert!(answer.body.contains("\"activity\""), "{}", answer.body);
    let activity_rows = answer.body.matches("\"sentence\":").count();
    assert!(
        activity_rows <= 50,
        "{} rows: {}",
        activity_rows,
        answer.body
    );
}

/// A photo needs a session: signed out gets the same 404 an unknown id
/// gets, and a second workspace's admin cannot see across the fence.
#[tokio::test]
async fn a_photo_hides_from_the_signed_out_and_the_foreign() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let admin_id = user_id(&app, "ada@izlek.sh").await;

    let answer = app
        .post_multipart(
            "/api/profile_photo",
            Some(&admin_cookie),
            &[],
            Some(("me.png", "image/png", &PNG)),
        )
        .await;
    assert_eq!(
        answer.location.as_deref(),
        Some("/settings?saved=profile_photo&section=profile")
    );

    let signed_out = app.get(&format!("/photo/{admin_id}"), None).await;
    assert_eq!(signed_out.status, StatusCode::NOT_FOUND);
}

/// An invite refusal reopens the Members section, where the message shows;
/// so does the mailed note on success.
#[tokio::test]
async fn an_invite_refusal_lands_on_the_members_section() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let dup = app
        .post_without_script(
            "/api/invite_member",
            Some(&admin_cookie),
            "http://localhost/settings?section=members",
            &[
                ("email", "emre@izlek.sh"),
                ("display_name", "Emre"),
                ("role", "member"),
            ],
        )
        .await;
    let location = dup.location.expect("no redirect");
    assert!(
        location.contains("on=invite_member") && location.contains("section=members"),
        "{location}"
    );

    let page = app.get(&location, Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("field-error"), "{html}");
    assert!(html.contains("emre@izlek.sh"), "{html}");
}

/// Seeds one decision and one accepted send directly on the store — the
/// engine's own timing is not what these notification tests are about — and
/// hands back the task and the send's id.
async fn a_task_with_a_notification(
    app: &App,
    admin_cookie: &str,
    subject: &str,
) -> (String, String) {
    let columns = columns_of(app).await;
    let task = a_task(app, admin_cookie, &columns[0], "Ship it").await;
    let admin_id = person_id(app, admin_cookie, &task, "Ada Lovelace").await;
    let workspace_id = app.workspace_id().await;
    let board = app.store.board(&workspace_id).await.unwrap().unwrap();
    let rule = app
        .store
        .create_mail_rule(
            &board.id,
            &Trigger::StatusBecomes(Some(columns[1].clone())),
            subject,
            Audience::Assignees,
            time::OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();
    let transition = match app
        .store
        .move_task(
            &task,
            &columns[0],
            &columns[1],
            &admin_id,
            time::OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
    {
        Moved::Recorded(transition) => transition,
        other => panic!("the move did not happen: {other:?}"),
    };
    let now = time::OffsetDateTime::now_utc();
    app.store
        .record_mail_decision(&rule.id, &transition.id, &task, MailOutcome::Owed, "", now)
        .await
        .unwrap();
    let send = app
        .store
        .claim_send(&rule.id, &transition.id, &task, "ada@izlek.sh", now, now)
        .await
        .unwrap()
        .unwrap();
    app.store.record_send_accepted(&send.id, now).await.unwrap();
    (task, send.id.clone())
}

/// A task's detail page shows its notifications: the recipient and a state
/// chip for a send its rules made, to anyone who can read the task — but the
/// rule's own name only to an admin.
#[tokio::test]
async fn a_tasks_notifications_show_the_recipient_and_the_rule_name_is_admin_only() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let (task, _send_id) =
        a_task_with_a_notification(&app, &admin_cookie, "Wraps up the sprint").await;

    let member_page = app
        .get(&format!("/?task={task}&tab=mail"), Some(&member))
        .await;
    let member_html = String::from_utf8_lossy(&member_page.bytes);
    assert!(member_html.contains("ada@izlek.sh"), "{member_html}");
    assert!(
        member_html.contains("rule-term-sent"),
        "no state chip on the member's page: {member_html}"
    );
    assert!(
        !member_html.contains("Wraps up the sprint"),
        "a member was shown the rule's name: {member_html}"
    );

    let admin_page = app
        .get(&format!("/?task={task}&tab=mail"), Some(&admin_cookie))
        .await;
    let admin_html = String::from_utf8_lossy(&admin_page.bytes);
    assert!(admin_html.contains("ada@izlek.sh"), "{admin_html}");
    assert!(admin_html.contains("Wraps up the sprint"), "{admin_html}");
}

/// Who a task mailed and whether it arrived belongs to everyone who can read
/// the task. Why the mail server refused belongs to the people who can do
/// something about it.
#[tokio::test]
async fn the_mail_servers_own_words_are_admin_only() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let (task, send_id) = a_task_with_a_notification(&app, &admin_cookie, "Sprint wrap").await;

    // The send failed, and the server said something about the account it
    // failed to authenticate.
    let now = time::OffsetDateTime::now_utc();
    app.store
        .record_send_refused(
            &send_id,
            "535 5.7.8 Username and Password not accepted for postmaster@dizey.sh",
            None,
            now,
        )
        .await
        .unwrap();

    let member_page = app
        .get(&format!("/?task={task}&tab=mail"), Some(&member))
        .await;
    let member_html = String::from_utf8_lossy(&member_page.bytes);
    // The fact of the failure is theirs to see.
    assert!(
        member_html.contains("ada@izlek.sh"),
        "the member lost the recipient too: {member_html}"
    );
    assert!(
        !member_html.contains("postmaster@dizey.sh") && !member_html.contains("535"),
        "a member was shown what the mail server said: {member_html}"
    );

    let admin_page = app
        .get(&format!("/?task={task}&tab=mail"), Some(&admin_cookie))
        .await;
    let admin_html = String::from_utf8_lossy(&admin_page.bytes);
    assert!(
        admin_html.contains("postmaster@dizey.sh"),
        "the admin cannot see why it failed either: {admin_html}"
    );
}

/// A task with no mail at all renders the same quiet empty line the other
/// blocks use, under the notifications heading.
#[tokio::test]
async fn a_task_with_no_mail_shows_the_quiet_notifications_line() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Nobody mails me").await;

    let page = app
        .get(&format!("/?task={task}&tab=mail"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("NOTIFICATIONS"), "{html}");
    assert!(html.contains("Nothing yet."), "{html}");
}

/// An admin's retry puts a failed send back in play — pending, due right
/// away — and the queue tab picks it back up.
#[tokio::test]
async fn an_admin_retries_a_failed_send_and_it_rejoins_the_queue() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let (task, send_id) = a_task_with_a_notification(&app, &admin_cookie, "Never got there").await;
    let now = time::OffsetDateTime::now_utc();
    app.store
        .record_send_refused(&send_id, "timeout", Some(now), now)
        .await
        .unwrap();

    let answer = app
        .post(
            "/api/retry_send",
            Some(&admin_cookie),
            &[("send_id", &send_id)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "null", "{}", answer.body);

    let sends = app.store.sends_for_task(&task, 10).await.unwrap();
    let reread = sends.iter().find(|s| s.id == send_id).unwrap();
    assert_eq!(reread.state, SendState::Pending);
    assert!(
        reread
            .next_attempt_at
            .is_some_and(|at| at <= time::OffsetDateTime::now_utc()),
        "the retry is not due right away: {reread:?}"
    );

    let logs = app
        .post("/api/current_logs", Some(&admin_cookie), &[])
        .await;
    assert!(
        logs.body.contains("\"recipient\":\"ada@izlek.sh\""),
        "the retried send did not rejoin the queue: {}",
        logs.body
    );
}

/// A member cannot retry a send, and the pages a member can reach never draw
/// a Retry button in the first place.
#[tokio::test]
async fn a_member_may_not_retry_a_send() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let (task, send_id) =
        a_task_with_a_notification(&app, &admin_cookie, "Member cannot touch this").await;
    let now = time::OffsetDateTime::now_utc();
    app.store
        .record_send_refused(&send_id, "timeout", Some(now), now)
        .await
        .unwrap();

    let answer = app
        .post("/api/retry_send", Some(&member), &[("send_id", &send_id)])
        .await;
    assert!(answer.body.contains("Forbidden"), "{}", answer.body);

    let sends = app.store.sends_for_task(&task, 10).await.unwrap();
    let reread = sends.iter().find(|s| s.id == send_id).unwrap();
    assert_eq!(
        reread.state,
        SendState::Failed,
        "the refused retry moved it anyway"
    );

    let member_page = app.get(&format!("/?task={task}"), Some(&member)).await;
    let member_html = String::from_utf8_lossy(&member_page.bytes);
    assert!(!member_html.contains("Retry"), "{member_html}");
}

/// Retrying a send that already went out changes nothing, and does not 500.
#[tokio::test]
async fn retrying_an_already_sent_send_changes_nothing() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let (task, send_id) =
        a_task_with_a_notification(&app, &admin_cookie, "Already on its way").await;

    let answer = app
        .post(
            "/api/retry_send",
            Some(&admin_cookie),
            &[("send_id", &send_id)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "null", "{}", answer.body);

    let sends = app.store.sends_for_task(&task, 10).await.unwrap();
    let reread = sends.iter().find(|s| s.id == send_id).unwrap();
    assert_eq!(
        reread.state,
        SendState::Sent,
        "a sent send was touched by a retry"
    );
}

// ---------------------------------------------------------------------------
// Send message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_admin_sends_a_message_to_one_member() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let _ = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let mert_id = app
        .store
        .user_by_email(&app.workspace_id().await, "emre@izlek.sh")
        .await
        .unwrap()
        .unwrap()
        .id;

    let answer = app
        .post(
            "/api/send_message",
            Some(&admin_cookie),
            &[
                ("to", &mert_id),
                ("subject", "Heads up"),
                ("body", "Standup moved to 10."),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("saved=send_message"),
        "{:?}",
        answer.location
    );

    let sends = app
        .store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let notices: Vec<_> = sends
        .iter()
        .filter(|send| send.kind == SendKind::Notice)
        .collect();
    assert_eq!(notices.len(), 1, "{sends:?}");
    let notice = notices[0];
    assert_eq!(notice.recipient, "emre@izlek.sh");
    assert_eq!(notice.subject.as_deref(), Some("Heads up"));
    assert_eq!(notice.body.as_deref(), Some("Standup moved to 10."));

    let queue = app.get("/logs?section=queue", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&queue.bytes);
    assert!(html.contains("Heads up"), "{html}");
}

#[tokio::test]
async fn an_admin_sends_a_message_to_everyone_and_not_to_themself() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let _ = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let _ = invited(&app, &admin_cookie, "quiet@izlek.sh", "Quiet", Role::Viewer).await;

    let answer = app
        .post(
            "/api/send_message",
            Some(&admin_cookie),
            &[
                ("to", "everyone"),
                ("subject", "All hands"),
                ("body", "Board meeting Friday."),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("saved=send_message"),
        "{:?}",
        answer.location
    );

    let sends = app
        .store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    let notices: Vec<_> = sends
        .iter()
        .filter(|send| send.kind == SendKind::Notice)
        .collect();
    assert_eq!(notices.len(), 2, "{sends:?}");
    assert!(
        notices.iter().all(|send| send.recipient != "ada@izlek.sh"),
        "{sends:?}"
    );
}

#[tokio::test]
async fn a_blank_subject_or_body_refuses_and_queues_nothing() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let _ = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/send_message",
            Some(&admin_cookie),
            &[
                ("to", "everyone"),
                ("subject", "   "),
                ("body", "Something"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=empty-subject&on=send_message"),
        "{:?}",
        answer.location
    );

    let answer = app
        .post(
            "/api/send_message",
            Some(&admin_cookie),
            &[("to", "everyone"), ("subject", "Subject"), ("body", "  ")],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=empty-body&on=send_message"),
        "{:?}",
        answer.location
    );

    let sends = app
        .store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    assert!(
        sends.iter().all(|send| send.kind != SendKind::Notice),
        "{sends:?}"
    );

    let location = answer.location.expect("no redirect");
    let page = app.get(&location, Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("field-error"), "{html}");
}

#[tokio::test]
async fn a_member_may_not_send_a_message_and_never_sees_the_panel() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/send_message",
            Some(&member),
            &[
                ("to", "everyone"),
                ("subject", "Sneaky"),
                ("body", "Should not send"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=forbidden&on=send_message"),
        "{:?}",
        answer.location
    );

    let sends = app
        .store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    assert!(
        sends.iter().all(|send| send.kind != SendKind::Notice),
        "{sends:?}"
    );

    let page = app.get("/settings", Some(&member)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(!html.contains("section=message"), "{html}");
    assert!(!html.contains("id=\"message\""), "{html}");
}

#[tokio::test]
async fn an_unknown_recipient_refuses_and_never_broadcasts() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let _ = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            "/api/send_message",
            Some(&admin_cookie),
            &[
                ("to", "not-a-real-id"),
                ("subject", "Subject"),
                ("body", "Body"),
            ],
        )
        .await;
    assert_ne!(
        answer.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        answer.body
    );
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal=no-such-member&on=send_message"),
        "{:?}",
        answer.location
    );

    let sends = app
        .store
        .mail_queue(10, izlek_core::store::FeedPage::Newest)
        .await
        .unwrap();
    assert!(
        sends.iter().all(|send| send.kind != SendKind::Notice),
        "{sends:?}"
    );
}

/// The markup inside a single tab anchor: found by its `tab=<slug>` href, up
/// to that anchor's own closing tag — for asserting a `detail-tab-count`
/// span is (or is not) inside the one tab it belongs to, not just somewhere
/// on the page.
fn tab_anchor<'a>(html: &'a str, slug: &str) -> &'a str {
    let needle = format!("&amp;tab={slug}\">");
    let (_, after) = html
        .split_once(&needle)
        .unwrap_or_else(|| panic!("no {slug} tab anchor: {html}"));
    after
        .split_once("</a>")
        .map(|(inner, _)| inner)
        .expect("unterminated tab anchor")
}

#[tokio::test]
async fn opening_the_task_with_no_tab_shows_the_task_region_and_the_strip() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Land on the task tab").await;

    let page = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(r#"class="detail-tabs""#),
        "no tab strip: {html}"
    );
    assert!(
        html.contains(r#"class="detail-fields""#),
        "no task fields grid: {html}"
    );
    assert!(
        !html.contains("comment-composer"),
        "composer shown on the task tab: {html}"
    );
}

#[tokio::test]
async fn each_tab_renders_only_its_own_region() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Walk every tab").await;

    let files = app
        .get(&format!("/?task={task}&tab=files"), Some(&admin_cookie))
        .await;
    let files_html = String::from_utf8_lossy(&files.bytes);
    assert!(
        files_html.contains("multipart/form-data"),
        "no files region: {files_html}"
    );
    assert!(
        !files_html.contains("comment-composer"),
        "composer shown on the files tab: {files_html}"
    );

    let comments = app
        .get(&format!("/?task={task}&tab=comments"), Some(&admin_cookie))
        .await;
    let comments_html = String::from_utf8_lossy(&comments.bytes);
    assert!(
        comments_html.contains("comment-composer"),
        "no composer on the comments tab: {comments_html}"
    );

    let activity = app
        .get(&format!("/?task={task}&tab=activity"), Some(&admin_cookie))
        .await;
    let activity_html = String::from_utf8_lossy(&activity.bytes);
    assert!(
        activity_html.contains("activity-stamp"),
        "no activity stamp: {activity_html}"
    );

    let mail = app
        .get(&format!("/?task={task}&tab=mail"), Some(&admin_cookie))
        .await;
    let mail_html = String::from_utf8_lossy(&mail.bytes);
    assert!(
        mail_html.contains("NOTIFICATIONS"),
        "no notifications heading: {mail_html}"
    );
}

#[tokio::test]
async fn a_tab_name_it_does_not_know_falls_back_to_the_task_region() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Land on garbage").await;

    let page = app
        .get(&format!("/?task={task}&tab=zzz"), Some(&admin_cookie))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(r#"class="detail-fields""#),
        "garbage tab did not fall back to the task region: {html}"
    );
}

#[tokio::test]
async fn the_tab_strip_counts_files_and_comments_and_is_silent_when_there_are_none() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Nothing attached yet").await;

    let page = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        !tab_anchor(&html, "files").contains("detail-tab-count"),
        "count shown with no files: {html}"
    );
    assert!(
        !tab_anchor(&html, "comments").contains("detail-tab-count"),
        "count shown with no comments: {html}"
    );

    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some(("spec.png", "image/png", &png)),
    )
    .await;
    app.post(
        "/api/post_comment",
        Some(&admin_cookie),
        &[("task_id", &task), ("body", "One comment")],
    )
    .await;

    let page = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        tab_anchor(&html, "files").contains(r#"class="detail-tab-count">1<"#),
        "no file count of 1: {html}"
    );
    assert!(
        tab_anchor(&html, "comments").contains(r#"class="detail-tab-count">1<"#),
        "no comment count of 1: {html}"
    );
}

#[tokio::test]
async fn the_title_renders_on_every_tab() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Carry my title everywhere").await;

    for tab in ["task", "files"] {
        let page = app
            .get(&format!("/?task={task}&tab={tab}"), Some(&admin_cookie))
            .await;
        let html = String::from_utf8_lossy(&page.bytes);
        assert!(
            html.contains(r#"class="detail-headline""#),
            "no headline on tab={tab}: {html}"
        );
        assert!(
            html.contains("Carry my title everywhere"),
            "no title text on tab={tab}: {html}"
        );
    }
}

#[tokio::test]
async fn a_member_who_may_only_read_still_gets_the_tab_strip() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(
        &app,
        &admin_cookie,
        "reads@izlek.sh",
        "Reader Only",
        Role::Member,
    )
    .await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "A member can still see this").await;

    let page = app.get(&format!("/?task={task}"), Some(&member)).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(r#"class="detail-tabs""#),
        "no tab strip for a member: {html}"
    );
}

/// A spreadsheet has no browser element behind it: İzlek reads the workbook
/// itself and lays the sheet out as a table, tab strip and all. The upload is
/// a real xlsx, so this covers the sniffer, the reader and the view together.
#[tokio::test]
async fn a_workbook_opens_as_a_table_and_its_other_sheet_is_one_link_away() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Read the book").await;

    let book = include_bytes!("fixtures/book.xlsx");
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some((
            "costs.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            book,
        )),
    )
    .await;
    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "costs.xlsx");

    let page = app
        .get(
            &format!("/?task={task}&file={file_id}"),
            Some(&admin_cookie),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("sheet-table"), "no sheet table: {html}");
    assert!(html.contains("Cable"), "a cell's text is missing: {html}");
    assert!(html.contains("1250"), "a number is missing: {html}");
    assert!(html.contains("Ledger"), "the sheet tab is missing: {html}");
    assert!(
        html.contains(&format!("file={file_id}&amp;sheet=1")),
        "no link to the second sheet: {html}"
    );

    // The tab strip is navigation, so the second sheet is a page of its own.
    let second = app
        .get(
            &format!("/?task={task}&file={file_id}&sheet=1"),
            Some(&admin_cookie),
        )
        .await;
    let second_html = String::from_utf8_lossy(&second.bytes);
    assert!(
        second_html.contains("Second sheet cell"),
        "the second sheet did not render: {second_html}"
    );

    // A hand-edited sheet index is the first sheet, not an error.
    let past_the_end = app
        .get(
            &format!("/?task={task}&file={file_id}&sheet=99"),
            Some(&admin_cookie),
        )
        .await;
    assert!(String::from_utf8_lossy(&past_the_end.bytes).contains("Cable"));
}

/// A status rule can watch the whole board: the column select's first option
/// sends no column, and the rule reads back as a sentence about status
/// changing rather than becoming one named column.
#[tokio::test]
async fn a_status_rule_may_watch_every_column() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/create_rule",
            Some(&admin_cookie),
            &[
                ("trigger", "status"),
                ("column_id", ""),
                ("subject", "It moved"),
                ("audience", "assignees"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.body, "null", "the rule was refused: {}", answer.body);

    let seen = app
        .post("/api/current_rules", Some(&admin_cookie), &[])
        .await;
    assert!(
        seen.body.contains("\"when\":\"When status changes\""),
        "{}",
        seen.body
    );
    assert!(
        seen.body.contains("\"column_id\":null"),
        "the every-column rule names no column: {}",
        seen.body
    );

    // The composer offers it: the column select carries the any option, and
    // it is the one selected while the rule names no column.
    let page = app.get("/rules", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("Any column"), "no any-column option: {html}");
}

/// The queue says when a retry fires, not just a bare moment: a stamp in the
/// future is labelled as the next try, and one already past says the send is
/// due rather than naming a time that has been and gone.
#[tokio::test]
async fn the_queue_says_when_the_next_try_fires() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let (_task, send_id) = a_task_with_a_notification(&app, &admin_cookie, "Never got there").await;
    let now = time::OffsetDateTime::now_utc();

    let later = now + time::Duration::hours(2);
    app.store
        .record_send_refused(&send_id, "timeout", Some(later), now)
        .await
        .unwrap();
    let logs = app
        .post("/api/current_logs", Some(&admin_cookie), &[])
        .await;
    assert!(
        logs.body.contains("next try"),
        "the queue does not say when the retry fires: {}",
        logs.body
    );

    // Once the moment has passed the row is waiting on the next sweep.
    app.store
        .record_send_refused(
            &send_id,
            "timeout",
            Some(now - time::Duration::hours(1)),
            now,
        )
        .await
        .unwrap();
    let logs = app
        .post("/api/current_logs", Some(&admin_cookie), &[])
        .await;
    assert!(
        logs.body.contains("\"next_attempt\":\"due\""),
        "a retry already owed does not say so: {}",
        logs.body
    );
}

/// An admin can set the address mail links point at — the box answers on
/// localhost and is reached through a proxy on a public name. A trailing
/// slash is trimmed, something that is not an origin is refused, and an empty
/// field puts the configured address back.
#[tokio::test]
async fn an_admin_sets_the_address_mail_links_point_at() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    async fn sender_with(app: &App, admin: &str, public_url: &str) -> Answer {
        app.post(
            "/api/save_sender",
            Some(admin),
            &[
                ("host", "smtp.fastmail.com"),
                ("port", "465"),
                ("username", "izlek"),
                ("password", SENDER_PASSWORD),
                ("from_name", "İzlek"),
                ("from_address", "izlek@izlek.sh"),
                ("public_url", public_url),
            ],
        )
        .await
    }

    let answer = sender_with(&app, &admin_cookie, "https://board.example/").await;
    assert!(
        answer
            .location
            .as_deref()
            .is_some_and(|location| !location.contains("refusal=")),
        "the address was refused: {:?}",
        answer.location
    );
    assert_eq!(
        app.store.workspace().await.unwrap().unwrap().public_url,
        Some("https://board.example".to_string()),
        "the trailing slash was kept"
    );

    // The field shows what is stored, so a reload does not blank it.
    let page = app
        .get("/settings?section=outgoing", Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("https://board.example"), "{html}");

    let refused = sender_with(&app, &admin_cookie, "board.example").await;
    assert!(
        refused
            .location
            .as_deref()
            .is_some_and(|location| location.contains("refusal=")),
        "a bare host was taken as an origin: {:?}",
        refused.location
    );
    assert_eq!(
        app.store.workspace().await.unwrap().unwrap().public_url,
        Some("https://board.example".to_string()),
        "the refused save wrote anyway"
    );

    sender_with(&app, &admin_cookie, "").await;
    assert_eq!(
        app.store.workspace().await.unwrap().unwrap().public_url,
        None,
        "an empty field did not clear the stored address"
    );
}

/// A sheet is read one window at a time and the pagers move it: down the rows
/// and across the columns, each a link like every other overlay state. The
/// window's own numbers and letters label the grid, so a page says where it
/// sits without being told.
#[tokio::test]
async fn a_big_sheet_pages_down_its_rows_and_across_its_columns() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Read the big book").await;

    let book = include_bytes!("fixtures/wide.xlsx");
    app.post_multipart(
        "/files",
        Some(&admin_cookie),
        &[("task_id", &task)],
        Some((
            "wide.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            book,
        )),
    )
    .await;
    let snapshot = app
        .post(
            "/api/fetch_task",
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "wide.xlsx");

    let first = app
        .get(
            &format!("/?task={task}&file={file_id}"),
            Some(&admin_cookie),
        )
        .await;
    let html = String::from_utf8_lossy(&first.bytes);
    assert!(
        html.contains("Col A"),
        "the first column is missing: {html}"
    );
    assert!(!html.contains("Col M"), "the window is not 12 wide: {html}");
    assert!(html.contains("A–L / 40"), "no column count: {html}");
    assert!(html.contains("1–8 / 8"), "no row count: {html}");
    assert!(
        html.contains(&format!("file={file_id}&amp;sheet=0&amp;rows=0&amp;cols=1")),
        "no step to the next columns: {html}"
    );

    // Stepping across lands on the next twelve columns, and the grid labels
    // them from where the window sits rather than from A.
    let across = app
        .get(
            &format!("/?task={task}&file={file_id}&sheet=0&rows=0&cols=1"),
            Some(&admin_cookie),
        )
        .await;
    let html = String::from_utf8_lossy(&across.bytes);
    assert!(html.contains("Col M"), "the second window is wrong: {html}");
    assert!(html.contains("M–X / 40"), "the count did not move: {html}");
    assert!(
        html.contains(&format!("file={file_id}&amp;sheet=0&amp;rows=0&amp;cols=0")),
        "no step back to the first columns: {html}"
    );

    // A window nobody links to is the empty one it names, not a redirect and
    // not the first page wearing another page's numbers.
    let nowhere = app
        .get(
            &format!("/?task={task}&file={file_id}&sheet=0&rows=99&cols=0"),
            Some(&admin_cookie),
        )
        .await;
    let html = String::from_utf8_lossy(&nowhere.bytes);
    assert!(html.contains("sheet-table"), "the grid is gone: {html}");
    assert!(!html.contains("Col A"), "row 4951 is not row 1: {html}");
}

// -- the live channel -------------------------------------------------------

/// Reads a live connection until it says `needle`, or until `patience` runs
/// out, and hands back everything it said. Bounded by the frame it is waiting
/// for rather than by a fixed sleep: the positive assertions return the moment
/// the announcement lands, and the negative ones spend their whole patience
/// proving the silence.
async fn live_until(
    response: topcoat::router::response::Response<Body>,
    needle: &str,
    patience: std::time::Duration,
) -> String {
    use http_body_util::BodyExt;

    let mut body = response.into_body();
    let mut heard = String::new();
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return heard;
        }
        match tokio::time::timeout(left, body.frame()).await {
            Err(_) => return heard,
            Ok(None) => return heard,
            Ok(Some(Err(_))) => return heard,
            Ok(Some(Ok(frame))) => {
                if let Some(chunk) = frame.data_ref() {
                    heard.push_str(&String::from_utf8_lossy(chunk));
                    if heard.contains(needle) {
                        return heard;
                    }
                }
            }
        }
    }
}

/// A browser nobody signed in gets no feed at all.
#[tokio::test]
async fn the_live_channel_refuses_a_signed_out_caller() {
    let app = App::open().await;
    admin(&app).await;
    let response = app.live_open(None).await;
    assert_eq!(response.status(), 401);
}

/// The role gate reaches the channel: a member is never told that an
/// admin-only surface moved, because being told is itself knowing something
/// about it. The assertion is on the raw bytes on purpose — the topic name
/// must not appear at all, not merely go unrendered.
#[tokio::test]
async fn the_live_channel_never_names_an_admin_surface_to_a_member() {
    let app = App::open().await;
    let boss = admin(&app).await;
    let member = invited(&app, &boss, "bo@izlek.sh", "Bo", Role::Member).await;

    let member_feed = app.live_open(Some(&member)).await;
    let admin_feed = app.live_open(Some(&boss)).await;

    // An admin-only write: a mail rule.
    let workspace_id = app.workspace_id().await;
    let board = app.store.board(&workspace_id).await.unwrap().unwrap().id;
    app.store
        .create_mail_rule(
            &board,
            &izlek_core::store::Trigger::Created,
            "Something happened",
            izlek_core::store::Audience::Board,
            time::OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();

    // The admin's stream is the clock: once it has the announcement, the
    // member's has had every chance to receive one too.
    let heard_by_admin = live_until(admin_feed, "rules", std::time::Duration::from_secs(10)).await;
    let heard_by_member =
        live_until(member_feed, "rules", std::time::Duration::from_millis(500)).await;

    assert!(
        !heard_by_member.contains("rules"),
        "a member was told about the rules: {heard_by_member}"
    );
    assert!(
        heard_by_admin.contains("rules"),
        "the admin was not told about the rules: {heard_by_admin}"
    );
}

/// A surface everybody can see is announced to everybody.
#[tokio::test]
async fn the_live_channel_announces_a_shared_surface_to_a_member() {
    let app = App::open().await;
    let boss = admin(&app).await;
    let member = invited(&app, &boss, "bo@izlek.sh", "Bo", Role::Member).await;
    let column = first_column(&app).await;

    let feed = app.live_open(Some(&member)).await;
    a_task(&app, &boss, &column, "Ship the exporter").await;
    let heard = live_until(feed, "board", std::time::Duration::from_secs(10)).await;

    assert!(heard.contains("board"), "no board announcement: {heard}");
}

/// The guard is what stops a soft navigation stacking a second `EventSource`
/// on the tab: `swap()` re-executes every script it swaps in, so the script
/// must refuse to run twice. Emitted once, and only once, per page.
#[tokio::test]
async fn the_live_script_is_emitted_once_and_guarded() {
    let app = App::open().await;
    let boss = admin(&app).await;
    let html = String::from_utf8(app.get("/", Some(&boss)).await.bytes).unwrap();

    assert_eq!(
        html.matches("__izlekLive").count(),
        2,
        "the live script should appear once, as a guard read and a guard set"
    );
    assert!(
        html.contains("EventSource('/api/live')"),
        "no live connection"
    );
}

/// A signed-out page opens no stream: `/api/live` refuses it, and a tab on the
/// sign-in screen would otherwise reconnect against that refusal forever.
#[tokio::test]
async fn a_signed_out_page_opens_no_live_connection() {
    let app = App::open().await;
    admin(&app).await;
    let html = String::from_utf8(app.get("/", None).await.bytes).unwrap();
    assert!(
        !html.contains("__izlekLive"),
        "the sign-in page carries the live script"
    );
}

/// Text that goes stale on the clock rather than on a write carries the mark
/// the tick looks for — a queued mail's next-try time is the case that started
/// this: nothing writes when the minute changes.
#[tokio::test]
async fn a_queued_mails_next_try_is_marked_for_the_tick() {
    let app = App::open().await;
    let boss = admin(&app).await;
    app.store
        .queue_notice(
            "bo@izlek.sh",
            "Your \u{130}zlek sign-in link",
            "body",
            time::OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let html = String::from_utf8(app.get("/logs?section=queue", Some(&boss)).await.bytes).unwrap();
    assert!(
        html.contains("data-tick"),
        "the queue's next-try stamp is not marked for the tick"
    );
}

/// A live update changes only what actually changed. It morphs the fetched
/// document onto the live one instead of replacing the body, which is what
/// leaves an open dropdown open, a caret where it was and a half-typed comment
/// intact — not because any of those is special-cased, but because nothing
/// touched them. Asserted here so a refactor back to a wholesale replace fails
/// loudly rather than quietly making the app unusable while anyone is typing.
#[tokio::test]
async fn a_live_update_morphs_rather_than_replacing_the_page() {
    let app = App::open().await;
    let boss = admin(&app).await;
    let html = String::from_utf8(app.get("/", Some(&boss)).await.bytes).unwrap();

    assert!(
        html.contains("function morph("),
        "no morph: the live path replaces the page"
    );
    assert!(
        html.contains("swap(t, r.url, false, false, true)"),
        "the live refresh does not ask for a morphing swap"
    );
    // The dropdown's trigger and panel are built by script and are in no
    // server response; a morph that did not know that would delete them.
    assert!(
        html.contains("function clientMade(") && html.contains("dd-panel"),
        "the morph would delete the dropdown's own nodes as strays"
    );
    // Navigations and form posts must still replace: restoring a sent comment
    // over the server's cleared box would look like the message never went.
    assert!(
        html.contains("document.body.replaceChildren()"),
        "the full-replace path is gone, so form posts would carry stale state"
    );
    assert!(
        html.contains("captureFields"),
        "the swap captures no field state"
    );
    assert!(
        html.contains("restoreFields"),
        "the swap restores no field state"
    );
}

/// A promise about the future is shown to the second. A retry that says 16:42
/// but fires at 16:42:47 reads as forty-seven seconds of broken clock to
/// whoever is watching one; the queue names the second it means.
#[tokio::test]
async fn the_queues_next_try_is_shown_to_the_second() {
    let app = App::open().await;
    let boss = admin(&app).await;
    app.store
        .queue_notice(
            "bo@izlek.sh",
            "Subject",
            "body",
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await
        .unwrap();

    let html = String::from_utf8(app.get("/logs?section=queue", Some(&boss)).await.bytes).unwrap();
    let (_, after) = html
        .split_once("rule-stamp")
        .expect("no next-try stamp on the queue");
    let stamp: String = after.chars().take(60).collect();
    // hh:mm:ss — three groups, not two.
    let seconds = stamp
        .split(|c: char| !c.is_ascii_digit() && c != ':')
        .find(|part| part.matches(':').count() == 2);
    assert!(
        seconds.is_some(),
        "the next-try stamp is not shown to the second: {stamp}"
    );
}

/// The chip reports what the server said, not what the form contains. A
/// refusal shows the server's own words, because "535 authentication failed"
/// is something an admin can act on and a grey chip is not.
#[tokio::test]
async fn a_refused_handshake_is_shown_with_what_the_server_said() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);

    let workspace = app.workspace_id().await;
    app.store
        .record_sender_check(
            &workspace,
            izlek_core::store::SenderCheck {
                at: time::OffsetDateTime::now_utc(),
                took_ms: 0,
                error: Some("535 authentication failed".into()),
            },
        )
        .await
        .unwrap();

    let html = String::from_utf8(
        app.get("/settings?section=outgoing", Some(&admin_cookie))
            .await
            .bytes,
    )
    .unwrap();
    assert!(html.contains("Refused"), "{html}");
    assert!(
        html.contains("535 authentication failed"),
        "the server's words are not on the page: {html}"
    );
    assert!(
        !html.contains("chip-connected"),
        "a refusal rendered as connected"
    );
}

/// A handshake that worked says when, because the claim is about a moment: a
/// password rotated an hour later is not something the panel can know.
#[tokio::test]
async fn a_passed_handshake_says_when_it_passed() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);

    let workspace = app.workspace_id().await;
    app.store
        .record_sender_check(
            &workspace,
            izlek_core::store::SenderCheck {
                at: time::OffsetDateTime::now_utc(),
                took_ms: 84,
                error: None,
            },
        )
        .await
        .unwrap();

    let html = String::from_utf8(
        app.get("/settings?section=outgoing", Some(&admin_cookie))
            .await
            .bytes,
    )
    .unwrap();
    assert!(html.contains("chip-connected"), "{html}");
    assert!(html.contains("Connected"), "{html}");
}

/// Dialling the mail server is an admin's business.
#[tokio::test]
async fn a_member_may_not_check_the_sender() {
    let app = App::open().await;
    let boss = admin(&app).await;
    let member = invited(&app, &boss, "bo@izlek.sh", "Bo", Role::Member).await;
    let answer = app.post("/api/check_sender", Some(&member), &[]).await;
    assert_ne!(
        answer.body, "null",
        "a member was allowed to dial the server"
    );
}

// --- tags -------------------------------------------------------------------

/// Makes a tag over HTTP and hands the store row back — the tests need the
/// id, and the mutation itself is still a real post.
async fn a_tag(app: &App, cookie: &str, name: &str) -> izlek_core::store::Tag {
    let answer = app
        .post("/api/create_tag", Some(cookie), &[("name", name)])
        .await;
    assert_eq!(answer.body, "null", "the tag was refused: {}", answer.body);
    let workspace_id = app.workspace_id().await;
    let board = app
        .store
        .board(&workspace_id)
        .await
        .unwrap()
        .expect("no board");
    app.store
        .tags(&board.id)
        .await
        .unwrap()
        .into_iter()
        .find(|tag| tag.name == name)
        .expect("the new tag is not on the board")
}

#[tokio::test]
async fn a_non_admin_cannot_create_rename_delete_or_move_a_tag() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "mem@izlek.sh", "Mem Ber", Role::Member).await;

    let create = app
        .post("/api/create_tag", Some(&member), &[("name", "Sneak")])
        .await;
    assert!(create.body.contains("Forbidden"), "{}", create.body);
    let rename = app
        .post(
            "/api/rename_tag",
            Some(&member),
            &[("tag_id", "t1"), ("name", "Sneak")],
        )
        .await;
    assert!(rename.body.contains("Forbidden"), "{}", rename.body);
    let delete = app
        .post("/api/delete_tag", Some(&member), &[("tag_id", "t1")])
        .await;
    assert!(delete.body.contains("Forbidden"), "{}", delete.body);
    let move_it = app
        .post(
            "/api/move_tag",
            Some(&member),
            &[("tag_id", "t1"), ("direction", "up")],
        )
        .await;
    assert!(move_it.body.contains("Forbidden"), "{}", move_it.body);
}

#[tokio::test]
async fn a_viewer_cannot_set_a_task_tag() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let viewer = invited(
        &app,
        &admin_cookie,
        "hush@izlek.sh",
        "Hush Rao",
        Role::Viewer,
    )
    .await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Label me not").await;
    let aurora = a_tag(&app, &admin_cookie, "Aurora").await;

    let answer = app
        .post(
            "/api/set_task_tag",
            Some(&viewer),
            &[("task_id", &task), ("tag_id", &aurora.id)],
        )
        .await;
    assert!(answer.body.contains("Forbidden"), "{}", answer.body);
}

#[tokio::test]
async fn the_task_modal_lists_every_tag_and_names_the_current_one() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Sort into color").await;
    let aurora = a_tag(&app, &admin_cookie, "Aurora").await;

    let workspace_id = app.workspace_id().await;
    let board = app
        .store
        .board(&workspace_id)
        .await
        .unwrap()
        .expect("no board");
    let default_id = app
        .store
        .tags(&board.id)
        .await
        .unwrap()
        .into_iter()
        .find(|tag| tag.is_default)
        .expect("no default tag")
        .id;
    let selected = |html: &str, id: &str| html.contains(&format!(r#"value="{id}" selected"#));

    let page = String::from_utf8(
        app.get(&format!("/?task={task}"), Some(&admin_cookie))
            .await
            .bytes,
    )
    .unwrap();
    assert!(
        page.contains(r#"name="tag_id""#),
        "no tag field in the modal: {page}"
    );
    assert!(
        page.contains("General"),
        "the default tag is not offered: {page}"
    );
    assert!(
        page.contains("Aurora"),
        "the new tag is not offered: {page}"
    );
    assert!(
        selected(&page, &default_id),
        "the current tag is not named: {page}"
    );
    assert!(
        !selected(&page, &aurora.id),
        "an unchosen tag reads as current: {page}"
    );

    let switched = app
        .post(
            "/api/set_task_tag",
            Some(&admin_cookie),
            &[("task_id", &task), ("tag_id", &aurora.id)],
        )
        .await;
    assert_eq!(
        switched.body, "null",
        "the switch was refused: {}",
        switched.body
    );

    let page = String::from_utf8(
        app.get(&format!("/?task={task}"), Some(&admin_cookie))
            .await
            .bytes,
    )
    .unwrap();
    assert!(
        selected(&page, &aurora.id),
        "the switch did not name Aurora: {page}"
    );
    assert!(
        !selected(&page, &default_id),
        "the old tag still reads current: {page}"
    );
}

#[tokio::test]
async fn the_board_tag_filter_narrows_cards_and_an_unknown_tag_falls_back_to_all() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let first = a_task(&app, &admin_cookie, &column, "Aurora work").await;
    let second = a_task(&app, &admin_cookie, &column, "Boreal work").await;
    let aurora = a_tag(&app, &admin_cookie, "Aurora").await;
    let boreal = a_tag(&app, &admin_cookie, "Boreal").await;

    let switched = app
        .post(
            "/api/set_task_tag",
            Some(&admin_cookie),
            &[("task_id", &second), ("tag_id", &boreal.id)],
        )
        .await;
    assert_eq!(
        switched.body, "null",
        "the switch was refused: {}",
        switched.body
    );

    let tagged = app
        .post(
            "/api/set_task_tag",
            Some(&admin_cookie),
            &[("task_id", &first), ("tag_id", &aurora.id)],
        )
        .await;
    assert_eq!(
        tagged.body, "null",
        "the first switch was refused: {}",
        tagged.body
    );

    let filtered = String::from_utf8(
        app.get(&format!("/?tag={}", aurora.id), Some(&admin_cookie))
            .await
            .bytes,
    )
    .unwrap();
    assert!(
        filtered.contains("Aurora work"),
        "the tagged card is gone: {filtered}"
    );
    assert!(
        !filtered.contains("Boreal work"),
        "the other card did not filter out: {filtered}"
    );

    let everything =
        String::from_utf8(app.get("/?tag=bogus", Some(&admin_cookie)).await.bytes).unwrap();
    assert!(
        everything.contains("Aurora work") && everything.contains("Boreal work"),
        "an unknown tag did not fall back to all: {everything}"
    );
    assert!(
        everything.contains("General"),
        "the default tag is not an option in the filter: {everything}"
    );
}

#[tokio::test]
async fn the_default_tag_cannot_be_deleted_and_ships_no_delete_button() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let _aurora = a_tag(&app, &admin_cookie, "Aurora").await;

    let workspace_id = app.workspace_id().await;
    let board = app
        .store
        .board(&workspace_id)
        .await
        .unwrap()
        .expect("no board");
    let default_id = app
        .store
        .tags(&board.id)
        .await
        .unwrap()
        .into_iter()
        .find(|tag| tag.is_default)
        .expect("no default tag")
        .id;

    let refused = app
        .post(
            "/api/delete_tag",
            Some(&admin_cookie),
            &[("tag_id", &default_id)],
        )
        .await;
    assert!(refused.body.contains("Unavailable"), "{}", refused.body);

    let page = String::from_utf8(app.get("/tags", Some(&admin_cookie)).await.bytes).unwrap();
    assert_eq!(
        page.matches(r#"action="/api/delete_tag""#).count(),
        1,
        "the default tag's row ships a delete control: {page}"
    );
    assert!(
        page.contains("Aurora"),
        "the created tag is not listed: {page}"
    );
}

/// A tag with cards on it stays: no delete control on its row, and the call
/// itself refused by name if somebody posts it anyway. Emptying it is what
/// makes it deletable.
#[tokio::test]
async fn a_tag_with_cards_on_it_cannot_be_deleted() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let aurora = a_tag(&app, &admin_cookie, "Aurora").await;
    let workspace_id = app.workspace_id().await;
    let board = app
        .store
        .board(&workspace_id)
        .await
        .unwrap()
        .expect("no board");
    let open = app.store.columns(&board.id).await.unwrap()[0].id.clone();
    let admin_id = app
        .store
        .users(&workspace_id)
        .await
        .unwrap()
        .into_iter()
        .find(|user| user.role == Role::Admin)
        .unwrap()
        .id;
    let task = a_task_by(&app, &board.id, &open, "Hold the door", &admin_id).await;
    app.store.set_task_tag(&task, &aurora.id).await.unwrap();

    let page = String::from_utf8(app.get("/tags", Some(&admin_cookie)).await.bytes).unwrap();
    assert_eq!(
        page.matches(r#"action="/api/delete_tag""#).count(),
        0,
        "a tag with a card on it still offers a delete: {page}"
    );
    assert!(
        page.contains(r#"<span class="tag-count">1</span>"#),
        "the row does not say how many cards are on it: {page}"
    );

    let refused = app
        .post(
            "/api/delete_tag",
            Some(&admin_cookie),
            &[("tag_id", &aurora.id)],
        )
        .await;
    assert!(
        refused.body.contains("TagInUse"),
        "the refusal does not name itself: {}",
        refused.body
    );
    assert_eq!(
        app.store.tags(&board.id).await.unwrap().len(),
        2,
        "the tag went anyway"
    );

    // Moved off it, the tag is deletable again — control and call both.
    let default_id = app
        .store
        .tags(&board.id)
        .await
        .unwrap()
        .into_iter()
        .find(|tag| tag.is_default)
        .unwrap()
        .id;
    app.store.set_task_tag(&task, &default_id).await.unwrap();
    let page = String::from_utf8(app.get("/tags", Some(&admin_cookie)).await.bytes).unwrap();
    assert_eq!(page.matches(r#"action="/api/delete_tag""#).count(), 1);
    let gone = app
        .post(
            "/api/delete_tag",
            Some(&admin_cookie),
            &[("tag_id", &aurora.id)],
        )
        .await;
    assert!(!gone.body.contains("TagInUse"), "{}", gone.body);
    assert_eq!(app.store.tags(&board.id).await.unwrap().len(), 1);
}

/// An open live stream must not be something the shutdown has to wait out.
///
/// The server stops accepting connections and then gives in-flight requests
/// thirty seconds to finish. A live stream intends to sit there for the whole
/// window, and every open tab is one, so before this the answer to Ctrl+C was
/// a thirty-second pause — measured at 30.00s with three tabs open, 0.003s
/// after. The stream is told, and it ends.
#[tokio::test]
async fn a_live_stream_ends_when_the_server_is_told_to_stop() {
    use http_body_util::BodyExt;

    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let mut body = app.live_open(Some(&admin_cookie)).await.into_body();

    // It is open and it is staying open: nothing has changed, so a read now
    // finds nothing to say.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), body.frame())
            .await
            .is_err(),
        "the stream ended on its own before anything asked it to"
    );

    app.stop.send(true).unwrap();

    // The window is ten seconds and the patience here is one, so a pass cannot
    // be the window expiring.
    let ended = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while let Some(Ok(_)) = body.frame().await {}
    })
    .await;
    assert!(ended.is_ok(), "the stream outlived the stop");
}
// --- profile page -----------------------------------------------------------

/// One task row id, for the profile fixture. Fixture setup goes through the
/// store directly, the way `record_sender_check`'s test does.
async fn a_task_by(
    app: &App,
    board_id: &str,
    column_id: &str,
    title: &str,
    creator: &str,
) -> String {
    app.store
        .create_task(izlek_core::store::NewTask {
            board_id,
            column_id,
            parent_id: None,
            title,
            description: "",
            deadline: None,
            clock_at: None,
            created_by: creator,
        })
        .await
        .unwrap()
        .row
        .id
}

/// Two tasks on Mem's plate, one finished under her, three she opened, four
/// comments — the numbers the profile page has to show: 2 / 1 / 3 / 4.
async fn profile_counts_fixture(app: &App, member: &str, admin: &str) {
    let workspace = app.workspace_id().await;
    let board = app
        .store
        .board(&workspace)
        .await
        .unwrap()
        .expect("no board");
    let columns = app.store.columns(&board.id).await.unwrap();
    let open = columns
        .iter()
        .find(|column| !column.is_done)
        .expect("no open column")
        .id
        .clone();
    let done = columns
        .iter()
        .find(|column| column.is_done)
        .expect("no done column")
        .id
        .clone();
    let now = time::OffsetDateTime::now_utc();

    let hold = a_task_by(app, &board.id, &open, "Hold the door", admin).await;
    app.store.assign_task(&hold, member).await.unwrap();
    let other = a_task_by(app, &board.id, &open, "Oil the hinges", admin).await;
    app.store.assign_task(&other, member).await.unwrap();
    let finished = a_task_by(app, &board.id, &open, "Paint the frame", admin).await;
    app.store.assign_task(&finished, member).await.unwrap();
    app.store
        .move_task(&finished, &open, &done, admin, now)
        .await
        .unwrap();
    for title in ["Sweep", "Mop", "Dust"] {
        a_task_by(app, &board.id, &open, title, member).await;
    }
    for _ in 0..4 {
        app.store
            .add_comment(&hold, member, "a note", now)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_member_may_read_another_members_profile_name_address_and_counts() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    invited(&app, &admin_cookie, "mem@izlek.sh", "Mem Ber", Role::Member).await;
    let reader = invited(
        &app,
        &admin_cookie,
        "ivy@izlek.sh",
        "Ivy Lear",
        Role::Member,
    )
    .await;
    let mem_id = user_id(&app, "mem@izlek.sh").await;
    let admin_id = app
        .store
        .users(&app.workspace_id().await)
        .await
        .unwrap()
        .into_iter()
        .find(|user| user.role == Role::Admin)
        .unwrap()
        .id;
    profile_counts_fixture(&app, &mem_id, &admin_id).await;

    let page = app.get(&format!("/people/{mem_id}"), Some(&reader)).await;
    assert_eq!(page.status.as_u16(), 200);
    let html = String::from_utf8(page.bytes).unwrap();
    assert!(html.contains("Mem Ber"), "{html}");
    assert!(html.contains("mem@izlek.sh"), "{html}");
    for count in [2, 1, 3, 4] {
        assert_eq!(
            html.matches(&format!(">{count}</dd>")).count(),
            1,
            "the {count} stat is not on the page once: {html}"
        );
    }
}

/// Every fact on a profile sits in a named cell, and the person who let this
/// member in is a link to their own page rather than a name in prose.
#[tokio::test]
async fn a_profile_names_its_fields_and_links_the_person_who_invited_them() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    invited(&app, &admin_cookie, "mem@izlek.sh", "Mem Ber", Role::Member).await;
    let mem_id = user_id(&app, "mem@izlek.sh").await;
    let admin_id = app
        .store
        .users(&app.workspace_id().await)
        .await
        .unwrap()
        .into_iter()
        .find(|user| user.role == Role::Admin)
        .unwrap()
        .id;

    let page = app
        .get(&format!("/people/{mem_id}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8(page.bytes).unwrap();
    for label in ["EMAIL", "JOINED", "LAST SEEN", "INVITED BY"] {
        assert!(
            html.contains(label),
            "the {label} field is not on the page: {html}"
        );
    }
    assert!(
        html.contains(&format!(r#"href="/people/{admin_id}""#)),
        "the inviter is not a way to their own page: {html}"
    );
    assert!(
        html.contains("avatar-xl"),
        "the profile picture is not the page's own size: {html}"
    );

    // The first account was invited by nobody, so it carries no such cell.
    let owner = app
        .get(&format!("/people/{admin_id}"), Some(&admin_cookie))
        .await;
    let owner_html = String::from_utf8(owner.bytes).unwrap();
    assert!(
        !owner_html.contains("INVITED BY"),
        "the owner was invited by somebody: {owner_html}"
    );
}

/// A face or a name in the task modal leads to the person it names.
#[tokio::test]
async fn a_comment_author_and_an_assignee_lead_to_their_profiles() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    invited(&app, &admin_cookie, "mem@izlek.sh", "Mem Ber", Role::Member).await;
    let mem_id = user_id(&app, "mem@izlek.sh").await;
    let workspace_id = app.workspace_id().await;
    let board = app.store.board(&workspace_id).await.unwrap().unwrap();
    let open = app.store.columns(&board.id).await.unwrap()[0].id.clone();
    let task = a_task_by(&app, &board.id, &open, "Hold the door", &mem_id).await;
    app.store.assign_task(&task, &mem_id).await.unwrap();
    app.store
        .add_comment(&task, &mem_id, "a note", time::OffsetDateTime::now_utc())
        .await
        .unwrap();

    let page = app
        .get(&format!("/?task={task}&tab=comments"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8(page.bytes).unwrap();
    assert!(
        html.matches(&format!(r#"href="/people/{mem_id}""#)).count() >= 2,
        "the comment's author and the assignee do not both lead anywhere: {html}"
    );
}

/// The board's project filter is typed into, like the pickers on the logs
/// page: a workspace with many tags is not a list to scroll.
#[tokio::test]
async fn the_project_filter_is_written_into() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let page = app.get("/", Some(&admin_cookie)).await;
    let html = String::from_utf8(page.bytes).unwrap();
    let filter = html
        .split("<select")
        .find(|chunk| chunk.contains(r#"name="tag""#))
        .unwrap_or_else(|| panic!("no project filter on the board: {html}"));
    assert!(
        filter.contains("data-search"),
        "the project filter cannot be typed into: {filter}"
    );
}

#[tokio::test]
async fn a_profile_is_not_found_signed_out_or_outside_the_workspace() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    invited(&app, &admin_cookie, "mem@izlek.sh", "Mem Ber", Role::Member).await;
    let mem_id = user_id(&app, "mem@izlek.sh").await;

    let signed_out = app.get(&format!("/people/{mem_id}"), None).await;
    assert_eq!(signed_out.status.as_u16(), 404, "never a 403, never a page");

    // A person who exists, but in another workspace, reads exactly like a
    // missing one — the id is not looked at twice.
    let other = App::open().await;
    // A workspace has to be claimed before it holds anybody to be a stranger.
    let _ = admin(&other).await;
    let stranger = other
        .store
        .users(&other.workspace_id().await)
        .await
        .unwrap()[0]
        .id
        .clone();
    let foreign = app
        .get(&format!("/people/{stranger}"), Some(&admin_cookie))
        .await;
    assert_eq!(foreign.status.as_u16(), 404);

    let missing = app.get("/people/nobody", Some(&admin_cookie)).await;
    assert_eq!(missing.status.as_u16(), 404);
}

#[tokio::test]
async fn only_your_own_profile_offers_the_edit_link() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    invited(&app, &admin_cookie, "mem@izlek.sh", "Mem Ber", Role::Member).await;
    let mem_id = user_id(&app, "mem@izlek.sh").await;
    let ivy = invited(
        &app,
        &admin_cookie,
        "ivy@izlek.sh",
        "Ivy Lear",
        Role::Member,
    )
    .await;
    let ivy_id = user_id(&app, "ivy@izlek.sh").await;

    let own = app.get(&format!("/people/{ivy_id}"), Some(&ivy)).await;
    let own_html = String::from_utf8(own.bytes).unwrap();
    assert!(
        own_html.contains(r#"href="/settings?section=profile""#),
        "no edit link on your own profile: {own_html}"
    );

    let theirs = app.get(&format!("/people/{mem_id}"), Some(&ivy)).await;
    let theirs_html = String::from_utf8(theirs.bytes).unwrap();
    assert!(
        !theirs_html.contains("settings?section=profile"),
        "someone else's profile grew an editor: {theirs_html}"
    );
}
// --- task clock -------------------------------------------------------------

#[tokio::test]
async fn a_clock_saved_on_a_task_renders_in_the_viewers_stored_timezone() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    // The viewer is still on UTC, so the value typed is the instant stored.
    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("deadline", "2026-09-02"),
                ("clock_hour", "11"),
                ("clock_minute", "00"),
            ],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );
    let facts = app.store.task(&task).await.unwrap().expect("task gone");
    assert!(facts.row.clock_at.is_some(), "the clock did not persist");

    let page = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(r#"aria-label="Hour" value="11""#)
            && html.contains(r#"aria-label="Minute" value="00""#),
        "{html}"
    );
    assert!(html.contains(">Sep 02 11:00<"), "{html}");

    // The same instant, read back by a viewer who stored UTC+03:00: the field
    // and its label both shift, the stored value does not.
    let saved = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("timezone", "UTC+03:00")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let shifted = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&shifted.bytes);
    assert!(
        html.contains(r#"aria-label="Hour" value="14""#)
            && html.contains(r#"aria-label="Minute" value="00""#),
        "{html}"
    );
    assert!(html.contains(">Sep 02 14:00<"), "{html}");
}

#[tokio::test]
async fn an_empty_time_clears_the_clock_and_a_clear_clears_both() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;
    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("deadline", "2026-09-02"),
                ("clock_hour", "11"),
                ("clock_minute", "00"),
            ],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    // A save that carries neither field — a title edit, a description
    // edit — says nothing about the moment at all: clock and day both stay.
    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[("task_id", &task), ("title", "Renamed")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );
    let facts = app.store.task(&task).await.unwrap().expect("task gone");
    assert!(
        facts.row.clock_at.is_some(),
        "an unrelated edit cleared the clock"
    );
    assert!(
        facts.row.deadline.is_some(),
        "an unrelated edit cleared the day"
    );
    assert_eq!(facts.row.title, "Renamed");

    // An empty time clears the clock and keeps the day.
    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[("task_id", &task), ("clock_hour", ""), ("clock_minute", "")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );
    let facts = app.store.task(&task).await.unwrap().expect("task gone");
    assert!(facts.row.clock_at.is_none(), "an empty time did not clear");
    assert!(
        facts.row.deadline.is_some(),
        "an empty time took the day with it"
    );

    let page = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains(">Sep 02<"), "{html}");
    // An empty time renders as no selected step at all — the bare "--".
    assert!(
        html.contains(r#"name="clock_hour""#) && html.contains(r#"name="clock_minute""#),
        "{html}"
    );
    assert!(!html.contains(">Sep 02 11:00<"), "{html}");
    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[("task_id", &task), ("deadline", ""), ("clock_hour", ""), ("clock_minute", "")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );
    let facts = app.store.task(&task).await.unwrap().expect("task gone");
    assert!(facts.row.deadline.is_none() && facts.row.clock_at.is_none());

    let page = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains(">no deadline</span>"), "{html}");
}

#[tokio::test]
async fn a_title_edit_keeps_the_clock() {
    // The title form posts neither moment field; its save must not be a
    // clock clear wearing a rename's clothes.
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("deadline", "2026-09-02"),
                ("clock_hour", "11"),
                ("clock_minute", "00"),
            ],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[("task_id", &task), ("title", "Renamed")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let facts = app.store.task(&task).await.unwrap().expect("task gone");
    assert_eq!(facts.row.title, "Renamed");
    assert!(
        facts.row.clock_at.is_some(),
        "the title edit cleared the clock"
    );
    assert!(
        facts.row.deadline.is_some(),
        "the title edit cleared the day"
    );
}

#[tokio::test]
async fn a_description_edit_keeps_the_clock() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("deadline", "2026-09-02"),
                ("clock_hour", "11"),
                ("clock_minute", "00"),
            ],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[("task_id", &task), ("description", "Now with words")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let facts = app.store.task(&task).await.unwrap().expect("task gone");
    assert_eq!(facts.description, "Now with words");
    assert!(
        facts.row.clock_at.is_some(),
        "the description edit cleared the clock"
    );
    assert!(
        facts.row.deadline.is_some(),
        "the description edit cleared the day"
    );
}

#[tokio::test]
async fn a_time_that_cannot_be_used_is_refused_and_saves_nothing() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    let answer = app
        .post_without_script(
            "/api/save_task",
            Some(&admin_cookie),
            &format!("/?task={task}"),
            &[
                ("task_id", task.as_str()),
                ("title", "Renamed"),
                ("deadline", "2026-09-02"),
                ("clock_hour", "not-a-time"),
            ],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-clock&on=save_task"),
        "{location}"
    );

    // The refused save is the whole save: the title and the day in the same
    // form did not go through either.
    let facts = app.store.task(&task).await.unwrap().expect("task gone");
    assert!(facts.row.clock_at.is_none(), "a bad time was stored");
    assert!(
        facts.row.deadline.is_none(),
        "the refused save kept the day"
    );
    assert_eq!(facts.row.title, "Ship it", "the refused save half-applied");

    let page = app.get(location, Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("That is not a time."), "{html}");

    // A time with no day anywhere — none posted, none on the task — has
    // nothing to sit on.
    let orphan = a_task(&app, &admin_cookie, &column, "Orphan").await;
    let answer = app
        .post_without_script(
            "/api/save_task",
            Some(&admin_cookie),
            &format!("/?task={orphan}"),
            &[
                ("task_id", orphan.as_str()),
                ("clock_hour", "16"),
                ("clock_minute", "20"),
            ],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-clock&on=save_task"),
        "{location}"
    );
    let facts = app.store.task(&orphan).await.unwrap().expect("task gone");
    assert!(facts.row.clock_at.is_none(), "a dayless time was stored");
    // And the mirror mistake: a minute with no hour is the same half of
    // nothing.
    let orphan2 = a_task(&app, &admin_cookie, &column, "Orphan Too").await;
    let answer = app
        .post_without_script(
            "/api/save_task",
            Some(&admin_cookie),
            &format!("/?task={orphan2}"),
            &[("task_id", orphan2.as_str()), ("clock_minute", "45")],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-clock&on=save_task"),
        "{location}"
    );
    let facts = app.store.task(&orphan2).await.unwrap().expect("task gone");
    assert!(facts.row.clock_at.is_none(), "a lone minute was stored");
}


#[tokio::test]
async fn a_card_wears_the_clock_chip_in_the_viewers_zone_instead_of_the_deadline() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    // The viewer reads in UTC+03:00, so the typed 14:00 is 11:00 stored and
    // 14:00 on the card again — the chip goes through the same zone both ways.
    let saved = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("timezone", "UTC+03:00")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let column = first_column(&app).await;
    let answer = app
        .post(
            "/api/create_task",
            Some(&admin_cookie),
            &[
                ("title", "Kickoff"),
                ("column_id", &column),
                ("deadline", "2026-09-02"),
                ("clock_hour", "14"),
                ("clock_minute", "00"),
            ],
        )
        .await;
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        answer.location
    );
    a_task(&app, &admin_cookie, &column, "No meeting").await;

    let page = app.get("/", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(r#"data-tick="" class="card-deadline">Sep 02 · 14:00<"#),
        "{html}"
    );
    assert!(
        html.contains("card-deadline card-deadline-none"),
        "the deadline-less card lost its own chip: {html}"
    );
}

#[tokio::test]
async fn a_time_set_on_an_existing_day_writes_both_columns() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    // A day first, then a time on its own: the day field was absent on the
    // second save, so the task's own day is what the moment sits on — and
    // the deadline column says the same day the clock does.
    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[("task_id", &task), ("deadline", "2026-09-02")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );
    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[("task_id", &task), ("clock_hour", "16"), ("clock_minute", "20")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );
    let facts = app.store.task(&task).await.unwrap().expect("task gone");
    assert!(facts.row.clock_at.is_some(), "the time did not persist");
    assert_eq!(
        facts.row.deadline.map(|day| day.to_string()),
        Some("2026-09-02".to_string()),
        "the time lost its day"
    );

    // Created with day and time in one form, the two columns agree too.
    let answer = app
        .post(
            "/api/create_task",
            Some(&admin_cookie),
            &[
                ("title", "Kickoff"),
                ("column_id", &column),
                ("deadline", "2026-09-10"),
                ("clock_hour", "9"),
                ("clock_minute", "30"),
            ],
        )
        .await;
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        answer.location
    );
    let board = app
        .store
        .board(&app.workspace_id().await)
        .await
        .unwrap()
        .expect("no board");
    let rows = app.store.tasks_for_board(&board.id).await.unwrap();
    let kickoff = rows
        .iter()
        .find(|row| row.title == "Kickoff")
        .expect("created task not on the board");
    assert_eq!(
        kickoff.deadline.map(|day| day.to_string()),
        Some("2026-09-10".to_string())
    );
    assert!(kickoff.clock_at.is_some());
}

#[tokio::test]
async fn the_reminder_knob_saves_renders_and_keeps_when_absent() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "10"),
                ("photo_limit_mb", "1"),
                ("allowed_file_types", "png, pdf"),
                ("mail_batch_minutes", "5"),
                ("reminder_minutes", "30"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("saved=save_limits"),
        "{:?}",
        answer.location
    );
    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.reminder_minutes, 30);

    let page = app
        .get("/settings?section=limits", Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(r#"name="reminder_minutes""#)
            && html.contains(r#"value="30""#)
            && html.contains("Reminder (minutes)"),
        "{html}"
    );

    // An older form's post, with no knob in it, leaves the stored choice.
    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "10"),
                ("photo_limit_mb", "1"),
                ("allowed_file_types", "png, pdf"),
                ("mail_batch_minutes", "5"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("saved=save_limits"),
        "{:?}",
        answer.location
    );
    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.reminder_minutes, 30, "an absent knob reset it");

    // Words are not minutes.
    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "10"),
                ("photo_limit_mb", "1"),
                ("allowed_file_types", "png, pdf"),
                ("mail_batch_minutes", "5"),
                ("reminder_minutes", "later"),
            ],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(
        location.contains("refusal=bad-limit&on=save_limits"),
        "{location}"
    );
    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.reminder_minutes, 30, "a bad knob half-applied");

    // Zero is the off switch, and it saves.
    let answer = app
        .post(
            "/api/save_limits",
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "10"),
                ("photo_limit_mb", "1"),
                ("allowed_file_types", "png, pdf"),
                ("mail_batch_minutes", "5"),
                ("reminder_minutes", "0"),
            ],
        )
        .await;
    assert!(
        answer
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("saved=save_limits"),
        "{:?}",
        answer.location
    );
    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.reminder_minutes, 0);
}

#[tokio::test]
async fn a_clock_rule_round_trips_and_renders_on_the_rules_page() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/create_rule",
            Some(&admin_cookie),
            &[
                ("trigger", "clock_set"),
                ("column_id", ""),
                ("subject", "Meeting soon"),
                ("audience", "board"),
            ],
        )
        .await;
    assert!(
        answer.status == StatusCode::SEE_OTHER
            && !answer
                .location
                .as_deref()
                .unwrap_or_default()
                .contains("refusal="),
        "{:?}",
        answer.location
    );

    let rules = app
        .post("/api/current_rules", Some(&admin_cookie), &[])
        .await;
    assert!(
        rules.body.contains(r#""when":"When a time is set""#),
        "{}",
        rules.body
    );
    assert!(
        rules.body.contains(r#""subject":"Meeting soon""#),
        "{}",
        rules.body
    );

    let page = app.get("/rules", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("When a time is set"), "{html}");
    assert!(html.contains(r#"<option value="clock_set""#), "{html}");
}

#[tokio::test]
async fn the_logs_feed_says_the_clock_in_its_own_words() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("deadline", "2026-09-02"),
                ("clock_hour", "11"),
                ("clock_minute", "00"),
            ],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let page = app.get("/logs?section=activity", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("set the time to"), "{html}");
    assert!(html.contains(r#"<option value="clock_set""#), "{html}");
}

#[tokio::test]
async fn the_datepicker_script_guards_a_panel_with_no_day_input() {
    // Every panel the server draws today carries a `.datepick-input`, but
    // the shared open handler renders whatever wears the datepick classes —
    // the guard is what keeps a future input-less panel from throwing
    // instead of drawing nothing.
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    let page = app
        .get(&format!("/?task={task}"), Some(&admin_cookie))
        .await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains("if (!input) { return; }"),
        "the datepicker script lost its no-day-input guard: {html}"
    );
    // The whole point of a day pick is the hidden input's new value: the
    // form posts nothing without it, and only a real browser runs this
    // script — so the write itself is asserted here, wire-level. A merge
    // once dropped exactly this line and every pick silently saved no day.
    assert!(
        html.contains("input.value = ymd ?"),
        "the datepicker script lost the day write: {html}"
    );
    // The moment field's panel: the grid's hidden day input and the time
    // box ride the same form.
    assert!(
        html.contains(r#"name="clock_hour""#) && html.contains(r#"name="clock_minute""#),
        "the moment boxes lost their names: {html}"
    );
    assert!(html.contains("datepick-input"), "{html}");
}

#[tokio::test]
async fn an_off_step_clock_renders_itself_not_a_rounded_step() {
    // A clock that never sat on a five-minute line — one that arrived
    // before the menu did — is shown exactly as stored: its own option is
    // appended, nothing is rounded onto a step.
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    let saved = app
        .post(
            "/api/save_task",
            Some(&admin_cookie),
            &[("task_id", &task), ("deadline", "2026-09-02"), ("clock_hour", "11"), ("clock_minute", "23")],
        )
        .await;
    assert!(
        !saved
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("refusal="),
        "{:?}",
        saved.location
    );

    let page = app.get(&format!("/?task={task}"), Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(r#"aria-label="Hour" value="11""#)
            && html.contains(r#"aria-label="Minute" value="23""#),
        "{html}"
    );
    assert!(html.contains(">Sep 02 11:23<"), "{html}");
}
