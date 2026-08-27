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
use izlek_core::store::{SendKind, Store, TursoStore};
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
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/debug/assets"))
}

/// A throwaway workspace: its own database file and its own router.
struct App {
    dir: PathBuf,
    router: Router,
    store: Arc<dyn Store>,
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
        let router = Router::builder()
            .discover()
            .layer(BodyLimit::max(izlek_web::settings::WIDEST_ATTACHMENT_MB as usize * 1024 * 1024).at("/files"))
            .cookies()
            .assets(AssetBundle::load_dir(asset_dir()).expect("run `topcoat asset bundle` before the http suite"))
            .app_context(accounts)
            .app_context(mail)
            .build();
        Self { dir, router, store }
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
        let router = Router::builder()
            .discover()
            .layer(BodyLimit::max(izlek_web::settings::WIDEST_ATTACHMENT_MB as usize * 1024 * 1024).at("/files"))
            .cookies()
            .assets(AssetBundle::load_dir(asset_dir()).expect("run `topcoat asset bundle` before the http suite"))
            .app_context(accounts)
            .app_context(Mail::sending(engine))
            .build();
        Self { dir, router, store }
    }

    /// The single workspace's id, read straight off the store: `TursoStore` is
    /// single-tenant, so there is no id to guess and no JSON endpoint needed
    /// just to hand it back.
    async fn workspace_id(&self) -> String {
        self.store.workspace().await.unwrap().expect("no workspace yet").id
    }

    /// Posts a form the way a hydrated caller does: `Accept: application/json`,
    /// no `Referer`. Every mutating `/api/*` route answers `303 See Other`
    /// regardless — [`Router::handle`] never follows a redirect, so the answer
    /// is read straight off this response, same as `oneshot` did before.
    async fn post(&self, path: &str, cookie: Option<&str>, form: &[(&str, &str)]) -> Answer {
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
        let response = self.router.handle(request.body(Body::from(body)).unwrap()).await;
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
        let response = self.router.handle(request.body(Body::from(body)).unwrap()).await;
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

        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            );
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        let response = self.router.handle(request.body(Body::from(body)).unwrap()).await;
        let mut answer = Answer::from_response(response).await;
        answer.session = None;
        answer
    }

    /// Gets a page or a download the way a browser does: no `Accept` header
    /// forcing JSON, an optional cookie, and the raw bytes back untouched — a
    /// download's body is not always UTF-8.
    async fn get(&self, path: &str, cookie: Option<&str>) -> Raw {
        let mut request = Request::builder().method("GET").uri(path);
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        let response = self.router.handle(request.body(Body::empty()).unwrap()).await;
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
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec();
        Raw { status, content_type, disposition, bytes }
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
                ("from_name", "Izlek"),
                ("from_address", "izlek@izlek.sh"),
            ],
        )
        .await;
    answer.status == StatusCode::SEE_OTHER
        && answer.location.as_deref().is_some_and(|location| !location.contains("refusal="))
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
    let sends = app.store.mail_queue(10).await.unwrap();
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
    columns_of(app).await.into_iter().next().expect("no columns on a fresh board")
}

/// Every column id on the board, in order, read straight off the store — see
/// [`first_column`] on why this bypasses HTTP.
async fn columns_of(app: &App) -> Vec<String> {
    let workspace_id = app.workspace_id().await;
    let board = app.store.board(&workspace_id).await.unwrap().expect("no board");
    app.store.columns(&board.id).await.unwrap().into_iter().map(|c| c.id).collect()
}

/// Makes a task and hands back its id, read straight off the store — there is
/// no `CurrentBoard` JSON call left to read it from (the board is a
/// server-rendered shard now); the mutation itself is still a real HTTP post.
async fn a_task(app: &App, cookie: &str, column: &str, title: &str) -> String {
    let answer = app
        .post("/api/create_task", Some(cookie), &[("title", title), ("column_id", column)])
        .await;
    assert_eq!(answer.body, "", "the task was refused: {}", answer.body);

    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id).await.unwrap().unwrap();
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

    let sends = app.store.mail_queue(10).await.unwrap();
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
        invites[0].body.as_deref().unwrap_or_default().contains("/join/"),
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
    let before = count(&app.store.mail_queue(10).await.unwrap());
    assert_eq!(before, 1);

    // `resend_link` has no hydrated action to answer with a value here: the
    // mailed address rides the redirect's query instead of a JSON body.
    let resent = app.post("/api/resend_link", Some(&admin_cookie), &[("user_id", &sena_id)]).await;
    assert_eq!(resent.status, StatusCode::SEE_OTHER, "{}", resent.body);
    let location = resent.location.expect("resend did not redirect");
    assert!(location.contains("mailed=sena%40izlek.sh"), "{location}");

    let after = count(&app.store.mail_queue(10).await.unwrap());
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
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=create_task"),
        "{:?}",
        answer.location
    );

    // And the refusal is not cosmetic: the board is still empty.
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id).await.unwrap().unwrap();
    assert!(
        board.columns.iter().flat_map(|c| &c.cards).all(|card| card.title != "Viewer should not get this"),
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
        !answer.location.as_deref().unwrap_or_default().contains("refusal="),
        "a create that worked said it was refused: {:?}",
        answer.location
    );

    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id).await.unwrap().unwrap();
    let card = board
        .columns
        .iter()
        .flat_map(|c| &c.cards)
        .find(|card| card.title == "Wire the deadline chip")
        .expect("new task is not on the board");
    assert_eq!(card.task_key, "DZ-01", "no key on the new task");
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
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=create_task"),
        "{:?}",
        answer.location
    );
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id).await.unwrap().unwrap();
    assert!(
        board.columns.iter().flat_map(|c| &c.cards).all(|card| card.title != "Wrong column"),
        "the refused task was written anyway"
    );
}

#[tokio::test]
async fn a_card_needs_a_title() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app).await;

    let answer = app
        .post("/api/create_task", Some(&admin), &[("title", "   "), ("column_id", &column)])
        .await;
    assert_eq!(answer.body, "");
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=empty-title&on=create_task"),
        "{:?}",
        answer.location
    );
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id).await.unwrap().unwrap();
    assert_eq!(board.columns.iter().flat_map(|c| &c.cards).count(), 0, "a blank title was stored anyway");
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
    assert!(!html.contains("board-stage"), "a signed-out browser was shown the board: {html}");
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
    let answer = app.post("/api/fetch_task", Some(&admin), &[("task_id", &task)]).await;
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

    let answer = app.post("/api/fetch_task", Some(&admin), &[("task_id", &task)]).await;
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
            &[("task_id", &second), ("other_id", &first), ("direction", "blocked_by")],
        )
        .await;
    assert_eq!(answer.body, "null", "the first link was refused: {}", answer.body);

    let answer = app
        .post(
            "/api/link_tasks",
            Some(&admin),
            &[("task_id", &first), ("other_id", &second), ("direction", "blocked_by")],
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
        &[("task_id", &second), ("other_id", &first), ("direction", "blocked_by")],
    )
    .await;

    // The same link back the other way, asked for by a browser that has no way
    // to read the answer's body.
    let answer = app
        .post_without_script(
            "/api/link_tasks",
            Some(&admin),
            &format!("http://izlek.test/?task={first}"),
            &[("task_id", &first), ("other_id", &second), ("direction", "blocked_by")],
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
            &[("task_id", &second), ("other_id", &first), ("direction", "blocked_by")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let location = answer.location.expect("a redirect with nowhere to go");
    assert!(!location.contains("refusal="), "a link that was made said it was refused: {location}");
}

#[tokio::test]
async fn a_task_id_from_nowhere_is_not_found() {
    let app = App::open().await;
    let admin = admin(&app).await;

    let answer = app
        .post("/api/fetch_task", Some(&admin), &[("task_id", "00000000-0000-0000-0000-000000000000")])
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

    let answer = app.post("/api/delete_task", Some(&viewer), &[("task_id", &task)]).await;
    assert_eq!(answer.body, "\"Forbidden\"");

    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id).await.unwrap().unwrap();
    assert!(
        board.columns.iter().flat_map(|c| &c.cards).any(|card| card.title == "Viewers cannot remove this"),
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
    assert_eq!(answer.location.as_deref(), Some("/"), "a deleted task's modal should not reopen");
}

#[tokio::test]
async fn a_member_may_delete_and_the_delete_is_soft() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "rae@izlek.sh", "Rae Okonkwo", Role::Member).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin, &column, "Mistyped in a hurry").await;

    // What it would cost is a read: it says so and writes nothing.
    let answer = app.post("/api/what_delete_costs", Some(&member), &[("task_id", &task)]).await;
    assert!(answer.body.contains("Mistyped in a hurry"), "{}", answer.body);
    let workspace_id = app.workspace_id().await;
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id).await.unwrap().unwrap();
    assert!(
        board.columns.iter().flat_map(|c| &c.cards).any(|card| card.title == "Mistyped in a hurry"),
        "asking cost deleted it"
    );

    let answer = app.post("/api/delete_task", Some(&member), &[("task_id", &task)]).await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    // Gone from the board, and gone from the detail: soft is not visible.
    let board = izlek_core::board::load(app.store.as_ref(), &workspace_id).await.unwrap().unwrap();
    assert!(
        board.columns.iter().flat_map(|c| &c.cards).all(|card| card.title != "Mistyped in a hurry"),
        "the task is still on the board"
    );
    let answer = app.post("/api/fetch_task", Some(&admin), &[("task_id", &task)]).await;
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
            &[("task_id", &task), ("from_column_id", &columns[0]), ("to_column_id", &columns[1])],
        )
        .await;

    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "");
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=move_card"),
        "{:?}",
        answer.location
    );

    // And nothing moved: the refusal is in the handler, not in the drawing.
    let answer = app.post("/api/fetch_task", Some(&admin), &[("task_id", &task)]).await;
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
            &[("task_id", &task), ("from_column_id", &columns[0]), ("to_column_id", &columns[1])],
        )
        .await;
    assert_eq!(answer.body, "");
    assert!(
        !answer.location.as_deref().unwrap_or_default().contains("refusal="),
        "a move that worked said it was refused: {:?}",
        answer.location
    );

    let answer = app.post("/api/fetch_task", Some(&admin), &[("task_id", &task)]).await;
    assert!(answer.body.contains("\"moved\""), "no move in the activity");
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
            &[("task_id", &task), ("from_column_id", &columns[0]), ("to_column_id", &columns[1])],
        )
        .await;
    assert_eq!(first.body, "");

    let second = app
        .post(
            "/api/move_card",
            Some(&member),
            &[("task_id", &task), ("from_column_id", &columns[0]), ("to_column_id", &columns[2])],
        )
        .await;
    assert_eq!(second.body, "");
    assert!(
        second.location.as_deref().unwrap_or_default().contains("refusal=moved-already&on=move_card"),
        "{:?}",
        second.location
    );

    // The winner's move stands, and there is exactly one crossing.
    let answer = app.post("/api/fetch_task", Some(&admin), &[("task_id", &task)]).await;
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
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=move_card"),
        "{:?}",
        answer.location
    );
    let answer = app.post("/api/fetch_task", Some(&admin), &[("task_id", &task)]).await;
    assert!(answer.body.contains(&columns[0]), "the forbidden move happened anyway");
}

// ---------------------------------------------------------------------------
// Settings: sign-out, profile, limits, sender, test mail, resend
// ---------------------------------------------------------------------------

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
        .post_without_script("/api/sign_out", Some(&cookie), "http://izlek.test/settings", &[])
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

    let answer = app.post("/api/save_profile", Some(&member), &[("display_name", "Emre Y")]).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert!(
        !answer.location.as_deref().unwrap_or_default().contains("refusal="),
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

    let answer = app.post("/api/save_profile", Some(&admin_cookie), &[("display_name", "   ")]).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(location.contains("refusal=empty-name&on=save_profile"), "{location}");

    let page = app.get(location, Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("Give yourself a name."), "{html}");
}

#[tokio::test]
async fn a_signed_out_browser_cannot_rename_anybody() {
    let app = App::open().await;
    let _ = admin(&app).await;

    let answer = app.post("/api/save_profile", None, &[("display_name", "Whoever")]).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=sign-in-first&on=save_profile"),
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
            &[("attachment_limit_mb", "400"), ("photo_limit_mb", "19"), ("allowed_file_types", "png")],
        )
        .await;
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=save_limits"),
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
            &[("attachment_limit_mb", "10"), ("photo_limit_mb", "1"), ("allowed_file_types", ".PNG, png, pdf")],
        )
        .await;
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("saved=save_limits"),
        "{:?}",
        answer.location
    );

    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.attachment_limit_bytes, 10 * 1024 * 1024);
    assert_eq!(workspace.photo_limit_bytes, 1024 * 1024);
    assert_eq!(workspace.allowed_file_types, vec!["png".to_string(), "pdf".to_string()]);
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
                &[("attachment_limit_mb", attachment), ("photo_limit_mb", photo), ("allowed_file_types", "")],
            )
            .await;
        let location = answer.location.as_deref().unwrap_or_default();
        assert!(location.contains("refusal=bad-limit&on=save_limits"), "{attachment}/{photo}: {location}");

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
            &[("attachment_limit_mb", "25"), ("photo_limit_mb", "2"), ("allowed_file_types", "../etc/passwd")],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(location.contains("refusal=bad-file-type&on=save_limits"), "{location}");

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
                ("from_name", "Izlek"),
                ("from_address", "emre@izlek.sh"),
            ],
        )
        .await;
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=save_sender"),
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
                ("from_name", "Izlek"),
                ("from_address", "nobody@izlek.sh"),
            ],
        )
        .await;
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=sign-in-first&on=save_sender"),
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
                ("from_name", "Izlek"),
                ("from_address", "izlek@izlek.sh"),
            ],
        )
        .await;
    assert!(
        !answer.location.as_deref().unwrap_or_default().contains("refusal="),
        "an empty password field was refused: {:?}",
        answer.location
    );

    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert_eq!(workspace.smtp_port, Some(587));
    assert!(workspace.smtp_password_set, "an empty field blanked the password");
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
                ("from_name", "Izlek"),
                ("from_address", "izlek@izlek.sh"),
            ],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(location.contains("refusal=bad-sender&on=save_sender"), "{location}");

    let page = app.get(location, Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("A password is needed the first time."), "{html}");
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
        ("port", &[("port", "0")], "A port is a number between 1 and 65535."),
        ("port", &[("port", "99999")], "A port is a number between 1 and 65535."),
        ("username", &[("username", "")], "Give the SMTP username."),
        ("from_address", &[("from_address", "board-at-izlek")], "That is not a from-address."),
        ("from_address", &[("from_address", "board@izlek")], "That is not a from-address."),
    ];
    for (field, overrides, expected) in bad {
        let mut form: Vec<(&str, &str)> = vec![
            ("host", "smtp.fastmail.com"),
            ("port", "587"),
            ("username", "izlek"),
            ("password", SENDER_PASSWORD),
            ("from_name", "Izlek"),
            ("from_address", "izlek@izlek.sh"),
        ];
        for (key, value) in *overrides {
            for slot in form.iter_mut() {
                if slot.0 == *key {
                    slot.1 = value;
                }
            }
        }
        let answer = app.post("/api/save_sender", Some(&admin_cookie), &form).await;
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
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=send_test_mail"),
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
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=send_test_mail"),
        "{:?}",
        answer.location
    );
}

#[tokio::test]
async fn a_signed_out_browser_may_not_press_the_test_button() {
    let app = App::open().await;

    let answer = app.post("/api/send_test_mail", None, &[]).await;
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=sign-in-first&on=send_test_mail"),
        "{:?}",
        answer.location
    );
}

#[tokio::test]
async fn testing_a_sender_that_was_never_filled_in_says_so_rather_than_sending() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app.post("/api/send_test_mail", Some(&admin_cookie), &[]).await;
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=bad-sender&on=send_test_mail"),
        "{:?}",
        answer.location
    );

    // Nothing was recorded either: the panel still has no test line to show.
    let workspace = app.store.workspace().await.unwrap().unwrap();
    assert!(workspace.sender_test.is_none(), "a test with no sender recorded a result");
}

#[tokio::test]
async fn a_member_who_posts_a_resend_anyway_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let mert = app
        .post("/api/invite_member", Some(&admin_cookie), &[("email", "mert@izlek.sh"), ("display_name", "Mert"), ("role", "member")])
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

    let answer = app.post("/api/resend_link", Some(&member), &[("user_id", &mert_id)]).await;
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("refusal=forbidden&on=resend_link"),
        "{:?}",
        answer.location
    );
    assert!(!answer.location.as_deref().unwrap_or_default().contains("mailed="), "{:?}", answer.location);
}

#[tokio::test]
async fn a_resent_link_opens_the_same_account() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let invitation = app
        .post("/api/invite_member", Some(&admin_cookie), &[("email", "mert@izlek.sh"), ("display_name", "Mert"), ("role", "member")])
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

    let answer = app.post("/api/resend_link", Some(&admin_cookie), &[("user_id", &mert_id)]).await;
    assert!(
        answer.location.as_deref().unwrap_or_default().contains("mailed=mert%40izlek.sh"),
        "{:?}",
        answer.location
    );
    let token = queued_join_token(&app, "mert@izlek.sh").await;

    let redeemed = app.post("/api/redeem_link", None, &[("token", &token), ("password", "lantern gravel spoon meadow")]).await;
    assert_eq!(redeemed.status, StatusCode::SEE_OTHER, "{}", redeemed.body);
    assert_eq!(redeemed.body, "null", "{}", redeemed.body);
    assert!(redeemed.session.is_some(), "the resent link signed nobody in");
}

#[tokio::test]
async fn an_invitation_names_the_admin_who_made_it_and_not_the_invitee() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post("/api/invite_member", Some(&admin_cookie), &[("email", "grace@izlek.sh"), ("display_name", "Grace Hopper"), ("role", "member")])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let token = queued_join_token(&app, "grace@izlek.sh").await;

    let answer = app.post("/api/invitation", None, &[("token", token.as_str())]).await;
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

    let page = app.get("/settings", Some(&admin_cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("smtp.fastmail.com"), "{html}");
    assert!(html.contains("izlek@izlek.sh"), "{html}");
    assert!(html.contains("Connected"), "{html}");
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
        .post("/api/invite_member", Some(&admin_cookie), &[("email", "mert@izlek.sh"), ("display_name", "Mert"), ("role", "member")])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);

    let page = app.get("/settings", Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("mert@izlek.sh"), "{html}");
    assert!(html.contains("Resend mail"), "the un-signed-in member has no resend control: {html}");
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
            &[("trigger", "status"), ("column_id", column_id), ("subject", subject), ("audience", "assignees")],
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

    let answer = app.post("/api/current_rules", Some(&admin_cookie), &[]).await;
    assert!(answer.body.contains("\"when\":\"When status becomes\""), "{}", answer.body);
    assert!(answer.body.contains("\"subject\":\"Task completed\""), "{}", answer.body);
    assert!(answer.body.contains("\"audience\":\"assignees\""), "{}", answer.body);
    // Nothing has been sent, and the row says so rather than nothing at all.
    assert!(answer.body.contains("\"last_sent\":null"), "{}", answer.body);
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
            &[("trigger", "status"), ("column_id", &column), ("subject", "   "), ("audience", "assignees")],
        )
        .await;
    assert!(answer.body.contains("EmptySubject"), "{}", answer.body);
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

    let seen = app.post("/api/current_rules", Some(&admin_cookie), &[]).await;
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
            &[("trigger", "status"), ("column_id", &column), ("subject", "Task completed"), ("audience", "everyone-everywhere")],
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

    let answer = app.post("/api/set_rule_enabled", Some(&admin_cookie), &[("rule_id", &rule), ("enabled", "false")]).await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    // The screen lists what exists, not what is live.
    let seen = app.post("/api/current_rules", Some(&admin_cookie), &[]).await;
    assert!(seen.body.contains("\"enabled\":false"), "{}", seen.body);
    assert!(seen.body.contains("\"subject\":\"Task completed\""), "{}", seen.body);
}

#[tokio::test]
async fn a_rule_id_this_workspace_does_not_own_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let stranger = Ulid::new().to_string();

    for path in ["/api/set_rule_enabled", "/api/delete_rule"] {
        let answer = app.post(path, Some(&admin_cookie), &[("rule_id", &stranger), ("enabled", "false")]).await;
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

    let answer = app.post("/api/delete_rule", Some(&admin_cookie), &[("rule_id", &rule)]).await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    let seen = app.post("/api/current_rules", Some(&admin_cookie), &[]).await;
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
                &[("trigger", "status"), ("column_id", &column), ("subject", "Mail everyone about me"), ("audience", "board")],
            )
            .await;
        assert!(written.body.contains("Forbidden"), "{}", written.body);

        let switched = app.post("/api/set_rule_enabled", Some(who), &[("rule_id", &rule), ("enabled", "false")]).await;
        assert!(switched.body.contains("Forbidden"), "{}", switched.body);

        let deleted = app.post("/api/delete_rule", Some(who), &[("rule_id", &rule)]).await;
        assert!(deleted.body.contains("Forbidden"), "{}", deleted.body);
    }

    // And the rule is untouched: still there, still on.
    let seen = app.post("/api/current_rules", Some(&admin_cookie), &[]).await;
    assert!(seen.body.contains("\"enabled\":true"), "{}", seen.body);
    assert!(!seen.body.contains("Mail everyone about me"), "{}", seen.body);
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
            vec![("trigger", "status"), ("column_id", column.as_str()), ("subject", "Task completed"), ("audience", "assignees")],
        ),
        ("/api/set_rule_enabled", vec![("rule_id", rule.as_str()), ("enabled", "false")]),
        ("/api/delete_rule", vec![("rule_id", rule.as_str())]),
    ] {
        let answer = app.post(path, None, &form).await;
        assert!(answer.body.contains("SignInFirst"), "{path}: {}", answer.body);
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

    let page = app.get(&format!("/?task={task}"), Some(&admin_cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("multipart/form-data"), "no multipart upload form on the detail page");
    assert!(html.contains(r#"action="/files""#), "the upload form does not post to /files");
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
    let viewer = invited(&app, &admin_cookie, "quiet@izlek.sh", "Quiet Reader", Role::Viewer).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Viewers cannot attach").await;

    let answer = app
        .post_multipart("/files", Some(&viewer), &[("task_id", &task)], Some(("note.txt", "text/plain", b"hello")))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some("/?refusal=forbidden&on=upload_file"));
}

#[tokio::test]
async fn a_member_uploads_a_file_and_the_chip_comes_back() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "mo@izlek.sh", "Mo Dubois", Role::Member).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Attach the spec").await;

    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    let answer = app
        .post_multipart("/files", Some(&member), &[("task_id", &task)], Some(("spec.png", "image/png", &png)))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some(format!("/?task={task}").as_str()));

    let snapshot = app.post("/api/fetch_task", Some(&member), &[("task_id", &task)]).await;
    let file_id = attachment_id_named(&snapshot.body, "spec.png");

    let page = app.get(&format!("/?task={task}"), Some(&member)).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains(&format!("/files/{file_id}")), "no href to the new file: {html}");
    assert!(html.contains("spec.png"));

    let download = app.get(&format!("/files/{file_id}"), Some(&member)).await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.bytes, png);
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
            &[("attachment_limit_mb", "1"), ("photo_limit_mb", "2"), ("allowed_file_types", "")],
        )
        .await;
    assert!(!answer.location.as_deref().unwrap_or_default().contains("refusal="), "{:?}", answer.location);

    let big = vec![0u8; 2 * 1024 * 1024];
    let answer = app
        .post_multipart("/files", Some(&admin_cookie), &[("task_id", &task)], Some(("big.bin", "application/octet-stream", &big)))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some(format!("/?task={task}&refusal=file-too-big&on=upload_file").as_str()));

    let snapshot = app.post("/api/fetch_task", Some(&admin_cookie), &[("task_id", &task)]).await;
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
            &[("attachment_limit_mb", "25"), ("photo_limit_mb", "2"), ("allowed_file_types", "png")],
        )
        .await;
    assert!(!answer.location.as_deref().unwrap_or_default().contains("refusal="), "{:?}", answer.location);

    let answer = app
        .post_multipart("/files", Some(&admin_cookie), &[("task_id", &task)], Some(("evil.exe", "application/octet-stream", b"MZ\x90\x00")))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some(format!("/?task={task}&refusal=file-type&on=upload_file").as_str()));
}

#[tokio::test]
async fn an_empty_allowed_list_lets_anything_through() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Whatever shows up").await;

    let answer = app
        .post_multipart("/files", Some(&admin_cookie), &[("task_id", &task)], Some(("anything.bin", "application/octet-stream", &[0x00, 0x01, 0x02])))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some(format!("/?task={task}").as_str()));
}

#[tokio::test]
async fn the_stored_type_is_what_the_bytes_are_not_what_the_upload_claimed() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Mislabeled on the way in").await;

    let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let answer = app
        .post_multipart("/files", Some(&admin_cookie), &[("task_id", &task)], Some(("liar.pdf", "application/pdf", &png)))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);

    let snapshot = app.post("/api/fetch_task", Some(&admin_cookie), &[("task_id", &task)]).await;
    let file_id = attachment_id_named(&snapshot.body, "liar.pdf");

    let download = app.get(&format!("/files/{file_id}"), Some(&admin_cookie)).await;
    assert_eq!(download.content_type.as_deref(), Some("image/png"));
}

#[tokio::test]
async fn a_file_name_that_is_a_path_is_kept_as_a_label() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Filename tries to escape").await;

    let answer = app
        .post_multipart("/files", Some(&admin_cookie), &[("task_id", &task)], Some(("../../etc/passwd", "text/plain", b"root:x:0:0")))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);

    let snapshot = app.post("/api/fetch_task", Some(&admin_cookie), &[("task_id", &task)]).await;
    assert!(snapshot.body.contains("\"name\":\"passwd\""), "the stored name still has a path in it: {}", snapshot.body);
    let file_id = attachment_id_named(&snapshot.body, "passwd");

    let page = app.get(&format!("/?task={task}"), Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(!html.contains("../../etc/passwd"), "the raw path leaked onto the chip: {html}");

    let download = app.get(&format!("/files/{file_id}"), Some(&admin_cookie)).await;
    let disposition = download.disposition.expect("no content-disposition on the download");
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
        .post_multipart("/files", Some(&admin_b), &[("task_id", &task_b)], Some(("theirs.png", "image/png", &[0x89, 0x50, 0x4E, 0x47])))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let snapshot = app_b.post("/api/fetch_task", Some(&admin_b), &[("task_id", &task_b)]).await;
    let file_id = attachment_id_named(&snapshot.body, "theirs.png");

    let answer = app_a.get(&format!("/files/{file_id}"), Some(&admin_a)).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_upload_without_a_file_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Nothing was chosen").await;

    let answer = app.post_multipart("/files", Some(&admin_cookie), &[("task_id", &task)], None).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some(format!("/?task={task}&refusal=no-file&on=upload_file").as_str()));
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
        .post_multipart("/files", Some(&member_a), &[("task_id", &task)], Some(("mine.png", "image/png", &[0x89, 0x50, 0x4E, 0x47])))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let answer = app
        .post_multipart("/files", Some(&member_b), &[("task_id", &task)], Some(("theirs.png", "image/png", &[0x89, 0x50, 0x4E, 0x47])))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);

    let snapshot = app.post("/api/fetch_task", Some(&admin_cookie), &[("task_id", &task)]).await;
    let file_a = attachment_id_named(&snapshot.body, "mine.png");
    let file_b = attachment_id_named(&snapshot.body, "theirs.png");

    let answer = app.post("/api/delete_file", Some(&member_b), &[("file_id", &file_a)]).await;
    assert_eq!(answer.body, "\"Forbidden\"", "{}", answer.body);

    let answer = app.post("/api/delete_file", Some(&member_a), &[("file_id", &file_a)]).await;
    assert_eq!(answer.body, "null", "the uploader was refused: {}", answer.body);

    let answer = app.post("/api/delete_file", Some(&admin_cookie), &[("file_id", &file_b)]).await;
    assert_eq!(answer.body, "null", "the admin was refused: {}", answer.body);

    let snapshot = app.post("/api/fetch_task", Some(&admin_cookie), &[("task_id", &task)]).await;
    assert!(snapshot.body.contains("\"files\":[]"), "{}", snapshot.body);
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// The id of the assignable person on a task whose display name matches.
async fn person_id(app: &App, cookie: &str, task_id: &str, name: &str) -> String {
    let answer = app.post("/api/fetch_task", Some(cookie), &[("task_id", task_id)]).await;
    let needle = format!("\"display_name\":\"{name}\"");
    let before = answer.body.split_once(&needle).map(|(head, _)| head).unwrap_or_else(|| panic!("no such person in {}", answer.body));
    before.rsplit_once("\"id\":\"").and_then(|(_, rest)| rest.split('"').next()).expect("no id before the display name").to_string()
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

    let assigned = app.post("/api/assign", Some(&admin_cookie), &[("task_id", &task), ("user_id", &mate_id)]).await;
    assert_eq!(assigned.body, "null", "{}", assigned.body);
    assert!(rule_written(&app, &admin_cookie, &columns[1], "Task completed").await);

    // Emre is the only assignee and Emre moves the card himself: the audience
    // empties out to nobody, and the decision says so rather than owing a
    // mail that would only tell him what he just did.
    let moved = app
        .post("/api/move_card", Some(&mate), &[("task_id", &task), ("from_column_id", &columns[0]), ("to_column_id", &columns[1])])
        .await;
    assert_eq!(moved.body, "");

    let snapshot = until_logs_contains(&app, &admin_cookie, "\"outcome\":\"nobody to mail\"").await;
    // The queue still carries Emre's invite mail — unrelated to this rule —
    // so the check is that the rule itself queued nothing, not an empty queue.
    assert!(!snapshot.contains("\"subject\":\"Task completed\""), "{}", snapshot);

    // The admin drops it back and moves it again: this time the mover is not
    // the assignee, so the rule owes Emre a mail. With no sender configured
    // the send is not a failure — it waits in the queue.
    let back = app
        .post("/api/move_card", Some(&admin_cookie), &[("task_id", &task), ("from_column_id", &columns[1]), ("to_column_id", &columns[0])])
        .await;
    assert_eq!(back.body, "");
    let forward = app
        .post("/api/move_card", Some(&admin_cookie), &[("task_id", &task), ("from_column_id", &columns[0]), ("to_column_id", &columns[1])])
        .await;
    assert_eq!(forward.body, "");

    // No sender means the send is held, not sent — the ledger stores that as
    // a failure with nothing spent, and the queue names the truth: held.
    let snapshot = until_logs_contains(&app, &admin_cookie, "\"recipient\":\"emre@izlek.sh\"").await;
    assert!(snapshot.contains("\"state\":\"held\""), "{}", snapshot);
    assert!(snapshot.contains("\"attempts\":0"), "{}", snapshot);
}

/// The `"at"` field of the activity row whose `"title"` matches, read out of
/// a `/api/current_logs` body the way `person_id` reads an id.
fn moment_for(body: &str, title: &str) -> String {
    let needle = format!("\"title\":\"{title}\"");
    let before = body.split_once(&needle).map(|(head, _)| head).unwrap_or_else(|| panic!("no such title in {body}"));
    before.rsplit_once("\"at\":\"").and_then(|(_, rest)| rest.split('"').next()).expect("no at before the title").to_string()
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
        !saved.location.as_deref().unwrap_or_default().contains("refusal="),
        "{:?}",
        saved.location
    );

    let shifted = app.post("/api/current_logs", Some(&admin_cookie), &[]).await;
    let shifted_at = moment_for(&shifted.body, "Ship it");

    assert_ne!(utc_at, shifted_at, "utc={utc_at} shifted={shifted_at}");
    assert_eq!(hour_of(&shifted_at), (hour_of(&utc_at) + 3) % 24, "utc={utc_at} shifted={shifted_at}");
}

/// The first `activity-stamp` span's text out of a task modal's HTML — the
/// same shape `moment_for` reads out of a `/api/current_logs` body.
fn activity_stamp_of(html: &str) -> &str {
    let (_, rest) = html
        .split_once(r#"class="activity-stamp">"#)
        .unwrap_or_else(|| panic!("no activity stamp in {html}"));
    rest.split_once('<').map(|(stamp, _)| stamp).expect("unterminated activity stamp")
}

#[tokio::test]
async fn a_task_modal_stamp_shifts_with_the_viewers_stored_timezone() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship it").await;

    let page = app.get(&format!("/?task={task}"), Some(&admin_cookie)).await;
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
        !saved.location.as_deref().unwrap_or_default().contains("refusal="),
        "{:?}",
        saved.location
    );

    let shifted_page = app.get(&format!("/?task={task}"), Some(&admin_cookie)).await;
    let shifted_html = String::from_utf8_lossy(&shifted_page.bytes);
    let shifted_at = activity_stamp_of(&shifted_html).to_string();

    assert_ne!(utc_at, shifted_at, "utc={utc_at} shifted={shifted_at}");
    assert_eq!(hour_of(&shifted_at), (hour_of(&utc_at) + 3) % 24, "utc={utc_at} shifted={shifted_at}");
}

#[tokio::test]
async fn an_unlisted_timezone_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            "/api/save_profile",
            Some(&admin_cookie),
            &[("display_name", "Ada Lovelace"), ("timezone", "Mars/Olympus_Mons")],
        )
        .await;
    let location = answer.location.as_deref().unwrap_or_default();
    assert!(location.contains("refusal=bad-zone&on=save_profile"), "{location}");
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
        !saved.location.as_deref().unwrap_or_default().contains("refusal="),
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
        !saved.location.as_deref().unwrap_or_default().contains("refusal="),
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
    assert_eq!(saved.location.as_deref(), Some("/settings?refusal=bad-language&on=save_profile"));
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
    assert!(location.contains("refusal=bad-theme&on=save_profile"), "{location}");
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
            &[("trigger", "status"), ("column_id", &column), ("subject", "New card"), ("audience", "board")],
        )
        .await;
    assert_eq!(written.body, "null", "{}", written.body);
    let rule = only_rule(&app, &admin_cookie).await;

    // Deniz creates the card, so the board audience (which excludes the
    // actor) resolves to the admin — the only other person on the board.
    let created = app
        .post("/api/create_task", Some(&member), &[("title", "Ship it"), ("column_id", &column)])
        .await;
    assert_eq!(created.body, "", "the task was refused: {}", created.body);

    let send = until_rule_send_to(&app, &rule, "ada@izlek.sh", 0).await;
    assert_eq!(send.recipient, "ada@izlek.sh");
}

#[tokio::test]
async fn the_settings_sidenav_offers_logs() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let page = app.get("/settings", Some(&admin_cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(html.contains("href=\"/logs\""), "{html}");
}

/// Polls the store until a `Rule` send for `rule_id` addressed to `recipient`
/// exists beyond the `already` count — the engine runs off the request in a
/// spawned task, so the row is not there yet when the triggering call
/// returns. Bounded so a send that never arrives fails the test instead of
/// hanging it.
async fn until_rule_send_to(app: &App, rule_id: &str, recipient: &str, already: usize) -> izlek_core::store::MailSend {
    for _ in 0..500 {
        let matching: Vec<_> = app
            .store
            .mail_queue(50)
            .await
            .unwrap()
            .into_iter()
            .filter(|send| send.kind == SendKind::Rule && send.rule_id.as_deref() == Some(rule_id) && send.recipient == recipient)
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
        .post("/api/create_rule", Some(&admin_cookie), &[("trigger", "commented"), ("column_id", ""), ("subject", "Someone commented"), ("audience", "creator")])
        .await;
    assert_eq!(created.body, "null", "{}", created.body);
    let rule = only_rule(&app, &admin_cookie).await;

    // A second member comments; the task's creator is the admin, not them.
    let commented = app.post("/api/post_comment", Some(&member), &[("task_id", &task), ("body", "Looks good")]).await;
    assert_eq!(commented.body, "null", "{}", commented.body);

    let send = until_rule_send_to(&app, &rule, "ada@izlek.sh", 0).await;
    assert_eq!(send.recipient, "ada@izlek.sh");
    assert!(
        app.store.mail_queue(50).await.unwrap().iter().all(|send| {
            !(send.kind == SendKind::Rule && send.rule_id.as_deref() == Some(rule.as_str()) && send.recipient == "deniz@izlek.sh")
        }),
        "the commenter was mailed instead of the creator"
    );
    let decisions = app.store.recent_mail_decisions(50).await.unwrap();
    assert!(
        decisions.iter().any(|decision| decision.rule_id == rule && matches!(decision.outcome, izlek_core::store::MailOutcome::Owed)),
        "the decisions ledger has no matched decision for the rule"
    );

    // The admin is put on the task so the rewritten rule's assignees audience
    // has someone to address once it fires on a rename.
    let admin_id = person_id(&app, &admin_cookie, &task, "Ada Lovelace").await;
    let assigned = app.post("/api/assign", Some(&admin_cookie), &[("task_id", &task), ("user_id", &admin_id)]).await;
    assert_eq!(assigned.body, "null", "{}", assigned.body);

    // The rule is rewritten in place: same id, new trigger, subject and
    // audience.
    let updated = app
        .post(
            "/api/update_rule",
            Some(&admin_cookie),
            &[("rule_id", &rule), ("trigger", "retitled"), ("column_id", ""), ("subject", "Renamed"), ("audience", "assignees")],
        )
        .await;
    assert_eq!(updated.body, "null", "the rewrite was refused: {}", updated.body);
    assert_eq!(only_rule(&app, &admin_cookie).await, rule, "a new rule was made instead of the old one rewritten");

    let seen = app.post("/api/current_rules", Some(&admin_cookie), &[]).await;
    assert!(seen.body.contains(&format!("\"id\":\"{rule}\"")), "{}", seen.body);
    assert!(seen.body.contains("\"enabled\":true"), "{}", seen.body);
    assert!(seen.body.contains("\"trigger_kind\":\"retitled\""), "{}", seen.body);
    assert!(seen.body.contains("\"subject\":\"Renamed\""), "{}", seen.body);

    // The second member renames the task; the rule now fires on retitle and
    // addresses the assignee — the admin — not the member who renamed it.
    let already = app
        .store
        .mail_queue(50)
        .await
        .unwrap()
        .iter()
        .filter(|send| send.kind == SendKind::Rule && send.rule_id.as_deref() == Some(rule.as_str()) && send.recipient == "ada@izlek.sh")
        .count();
    let renamed = app.post("/api/save_task", Some(&member), &[("task_id", &task), ("title", "Ship the redesigned picker")]).await;
    assert_eq!(renamed.body, "null", "{}", renamed.body);

    until_rule_send_to(&app, &rule, "ada@izlek.sh", already).await;
    assert!(
        app.store.mail_queue(50).await.unwrap().iter().all(|send| {
            !(send.kind == SendKind::Rule && send.rule_id.as_deref() == Some(rule.as_str()) && send.recipient == "deniz@izlek.sh")
        }),
        "the renamer was mailed instead of being excluded as the actor"
    );

    let logs = until_logs_contains(&app, &admin_cookie, "\"subject\":\"Renamed\"").await;
    assert!(logs.contains("\"recipient\":\"ada@izlek.sh\""), "{}", logs);
}
