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
use uuid::Uuid;

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
        let dir = std::env::temp_dir().join(format!("izlek-http-{}", Uuid::new_v4()));
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
        let dir = std::env::temp_dir().join(format!("izlek-http-{}", Uuid::new_v4()));
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
    // FINDING: board.rs's `create_task` route discards `create_task_shared`'s
    // refusal (`let _ = create_task_shared(...).await?;`) and its `Redirect`
    // has no `Json` component at all, unlike auth.rs/detail.rs's — so neither
    // a hydrated caller nor a scriptless browser ever learns *why* a create
    // was refused, only that they were sent back. The old test's
    // `assert_eq!(answer.body, "\"Forbidden\"")` has no wire shape to land on
    // any more; the refusal is checked by its side effect below instead.
    assert_eq!(answer.body, "");

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
    // create_task carries no body at all (see FINDING above); "" is success
    // here just as much as it is a swallowed refusal — the side effect below
    // is what actually tells the two apart.
    assert_eq!(answer.body, "");

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
    // See FINDING on `a_viewer_who_posts_to_create_task_anyway_is_refused`:
    // create_task never surfaces a refusal in its body.
    assert_eq!(answer.body, "");
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
    // See FINDING on `a_viewer_who_posts_to_create_task_anyway_is_refused`:
    // move_card shares create_task's bodyless `Redirect` and never surfaces a
    // refusal either.
    assert_eq!(answer.body, "");

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
    assert_eq!(answer.body, "", "move_card never carries a body (see FINDING)");

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
    // Same FINDING: move_card's `Redirect` carries no body, so the
    // stale-drop refusal is checked below, on the activity log, not here.
    assert_eq!(second.body, "");

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
    let answer = app.post("/api/fetch_task", Some(&admin), &[("task_id", &task)]).await;
    assert!(answer.body.contains(&columns[0]), "the forbidden move happened anyway");
}
