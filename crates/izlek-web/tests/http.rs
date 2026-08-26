//! The app driven the way a browser drives it: real router, real handlers, real
//! session cookie.
//!
//! The point of this binary is the guards. A button the UI does not draw proves
//! nothing — a Viewer who posts to the mutation endpoint anyway must be refused
//! by the handler, and that is what these tests call.
//!
//! New HTTP tests belong in this file rather than a new `tests/*.rs`: one test
//! binary links and runs once.
#![cfg(feature = "ssr")]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use izlek_core::Role;
use izlek_core::accounts::Accounts;
use izlek_core::store::{SendKind, Store, TursoStore};
use izlek_web::server::SESSION_COOKIE;
use leptos::prelude::LeptosOptions;
use leptos::server_fn::ServerFn;
use tower::ServiceExt;
use uuid::Uuid;

/// A throwaway workspace: its own database file and its own router.
struct App {
    dir: PathBuf,
    router: Router,
    store: Arc<dyn Store>,
}

impl App {
    async fn open() -> Self {
        let dir = std::env::temp_dir().join(format!("izlek-http-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store: Arc<dyn Store> = Arc::new(
            TursoStore::open(dir.join("izlek.db").to_str().unwrap())
                .await
                .unwrap(),
        );
        let options = LeptosOptions::builder().output_name("izlek").build();
        let router = izlek_web::server::router(
            Accounts::new(store.clone(), "http://127.0.0.1:3000"),
            izlek_web::server::Mail::silent(),
            options,
        );
        Self { dir, router, store }
    }

    /// Like `open`, but with a live mail engine reading the workspace's SMTP
    /// settings, so a transition actually reaches the ledger instead of the
    /// silent no-op `open`'s router hands every crossing.
    async fn open_with_mail() -> Self {
        let dir = std::env::temp_dir().join(format!("izlek-http-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store: Arc<dyn izlek_core::store::Store> = Arc::new(
            TursoStore::open(dir.join("izlek.db").to_str().unwrap())
                .await
                .unwrap(),
        );
        let engine = Arc::new(izlek_core::MailEngine::new(
            store.clone(),
            Arc::new(izlek_web::smtp::WorkspaceSmtp::new(store.clone())),
            "https://izlek.sh",
        ));
        let options = LeptosOptions::builder().output_name("izlek").build();
        let router = izlek_web::server::router(
            Accounts::new(store.clone(), "https://izlek.sh"),
            izlek_web::server::Mail::sending(engine),
            options,
        );
        Self { dir, router, store }
    }

    /// Posts a form to a server function, as the browser does, and returns the
    /// status, the JSON body and any session cookie the answer set.
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
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
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
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Answer {
            status,
            session,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            location,
        }
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
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Answer {
            status,
            session: None,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            location,
        }
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
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Answer {
            status,
            session: None,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            location,
        }
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
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
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
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
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

fn path<F: ServerFn>() -> &'static str {
    F::PATH
}

/// What the tests type into the password field. It must never come back out.
const SENDER_PASSWORD: &str = "cavalry-battery-hinge-40";

/// Fills in the sender panel the way an admin would, and reports whether the
/// server accepted it.
async fn sender_saved(app: &App, admin: &str) -> bool {
    let answer = app
        .post(
            path::<izlek_web::settings::SaveSender>(),
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
    answer.status == StatusCode::OK && answer.body == "null"
}

/// Claims the workspace and returns the admin's session cookie.
async fn admin(app: &App) -> String {
    let answer = app
        .post(
            path::<izlek_web::auth::ClaimWorkspace>(),
            None,
            &[
                ("display_name", "Ada Lovelace"),
                ("email", "ada@izlek.sh"),
                ("password", "correct horse battery staple"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
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
            path::<izlek_web::auth::InviteMember>(),
            Some(admin),
            &[("email", email), ("display_name", name), ("role", role)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let token = queued_join_token(app, email).await;

    let answer = app
        .post(
            path::<izlek_web::auth::RedeemLink>(),
            None,
            &[
                ("token", &token),
                ("password", "lantern gravel spoon meadow"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "null", "first sign-in was refused");
    answer.session.expect("first sign-in set no session cookie")
}

/// Inviting a member never hands the join link to the browser: the address
/// comes back, and the link travels only in the mail the invitee gets.
#[tokio::test]
async fn adding_a_member_mails_them_the_link() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::auth::InviteMember>(),
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
            path::<izlek_web::auth::InviteMember>(),
            Some(&admin_cookie),
            &[
                ("email", "sena@izlek.sh"),
                ("display_name", "Sena"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(invitation.status, StatusCode::OK, "{}", invitation.body);

    let list = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    let sena_id = list
        .body
        .split_once("sena@izlek.sh")
        .map(|(before, _)| before)
        .and_then(|before| before.rsplit_once("\"id\":\""))
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no member id")
        .to_string();

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

    let resent = app
        .post(
            path::<izlek_web::settings::ResendLink>(),
            Some(&admin_cookie),
            &[("user_id", &sena_id)],
        )
        .await;
    assert_eq!(resent.body, "{\"Ok\":\"sena@izlek.sh\"}", "{}", resent.body);

    let after = count(&app.store.mail_queue(10).await.unwrap());
    assert_eq!(after, 2);
}

/// The id of the board's first column, read the way the board page reads it.
async fn first_column(app: &App, cookie: &str) -> String {
    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), Some(cookie), &[])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    answer
        .body
        .split_once("\"columns\":[{\"column\":{\"id\":\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no column in the board")
        .to_string()
}

#[tokio::test]
async fn a_viewer_who_posts_to_create_task_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "quiet@izlek.sh", "Quiet Reader", Role::Viewer).await;
    let column = first_column(&app, &admin).await;

    let answer = app
        .post(
            path::<izlek_web::board::CreateTask>(),
            Some(&viewer),
            &[
                ("title", "Viewer should not get this"),
                ("column_id", &column),
            ],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "\"Forbidden\"");

    // And the refusal is not cosmetic: the board is still empty.
    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(
        !answer.body.contains("Viewer should not get this"),
        "the refused task was written anyway: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_member_may_create_a_task() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@izlek.sh", "Mo Dubois", Role::Member).await;
    let column = first_column(&app, &admin).await;

    let answer = app
        .post(
            path::<izlek_web::board::CreateTask>(),
            Some(&member),
            &[("title", "Wire the deadline chip"), ("column_id", &column)],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(answer.body.contains("Wire the deadline chip"));
    assert!(answer.body.contains("DZ-01"), "no key on the new task");
}

#[tokio::test]
async fn a_task_cannot_be_dropped_into_another_workspaces_column() {
    let app = App::open().await;
    let admin = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::board::CreateTask>(),
            Some(&admin),
            &[
                ("title", "Wrong column"),
                ("column_id", "00000000-0000-0000-0000-000000000000"),
            ],
        )
        .await;
    assert_eq!(answer.body, "\"Forbidden\"");
}

#[tokio::test]
async fn a_card_needs_a_title() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app, &admin).await;

    let answer = app
        .post(
            path::<izlek_web::board::CreateTask>(),
            Some(&admin),
            &[("title", "   "), ("column_id", &column)],
        )
        .await;
    assert_eq!(answer.body, "\"EmptyTitle\"");
}

#[tokio::test]
async fn the_board_is_not_readable_without_a_session() {
    let app = App::open().await;
    let _admin = admin(&app).await;

    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), None, &[])
        .await;
    assert_eq!(answer.body, "{\"Err\":\"SignInFirst\"}");
}

/// Makes a task and hands back its id, read off the board the way the browser
/// would.
async fn a_task(app: &App, cookie: &str, column: &str, title: &str) -> String {
    let answer = app
        .post(
            path::<izlek_web::board::CreateTask>(),
            Some(cookie),
            &[("title", title), ("column_id", column)],
        )
        .await;
    assert_eq!(answer.body, "null", "the task was refused: {}", answer.body);

    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), Some(cookie), &[])
        .await;
    let needle = format!("\"title\":\"{title}\"");
    let before = answer
        .body
        .split_once(&needle)
        .map(|(head, _)| head)
        .expect("the new task is not on the board");
    before
        .rsplit_once("{\"id\":\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no id on the new task")
        .to_string()
}

#[tokio::test]
async fn a_viewer_who_posts_a_comment_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "eyes@izlek.sh", "Ida Eyes", Role::Viewer).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Ship the detail modal").await;

    let answer = app
        .post(
            path::<izlek_web::detail::PostComment>(),
            Some(&viewer),
            &[("task_id", &task), ("body", "Viewers cannot say this")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "\"Forbidden\"");

    // The refusal is not cosmetic: nothing was written.
    let answer = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
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
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Wire the picker").await;

    let answer = app
        .post(
            path::<izlek_web::detail::PostComment>(),
            Some(&member),
            &[("task_id", &task), ("body", "Picker is narrow on purpose")],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    let answer = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
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
    let column = first_column(&app, &admin).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    let answer = app
        .post(
            path::<izlek_web::detail::LinkTasks>(),
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
            path::<izlek_web::detail::LinkTasks>(),
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
    let column = first_column(&app, &admin).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    app.post(
        path::<izlek_web::detail::LinkTasks>(),
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
            path::<izlek_web::detail::LinkTasks>(),
            Some(&admin),
            &format!("http://izlek.test/?task={first}"),
            &[
                ("task_id", &first),
                ("other_id", &second),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::FOUND);
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
    let column = first_column(&app, &admin).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    let answer = app
        .post_without_script(
            path::<izlek_web::detail::LinkTasks>(),
            Some(&admin),
            "http://izlek.test/",
            &[
                ("task_id", &second),
                ("other_id", &first),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::FOUND);
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
            path::<izlek_web::detail::FetchTask>(),
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
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Viewers cannot remove this").await;

    let answer = app
        .post(
            path::<izlek_web::detail::DeleteTask>(),
            Some(&viewer),
            &[("task_id", &task)],
        )
        .await;
    assert_eq!(answer.body, "\"Forbidden\"");

    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(
        answer.body.contains("Viewers cannot remove this"),
        "the refused delete happened anyway: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_member_may_delete_and_the_delete_is_soft() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "rae@izlek.sh", "Rae Okonkwo", Role::Member).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Mistyped in a hurry").await;

    // What it would cost is a read: it says so and writes nothing.
    let answer = app
        .post(
            path::<izlek_web::detail::WhatDeleteCosts>(),
            Some(&member),
            &[("task_id", &task)],
        )
        .await;
    assert!(
        answer.body.contains("Mistyped in a hurry"),
        "{}",
        answer.body
    );
    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(
        answer.body.contains("Mistyped in a hurry"),
        "asking cost deleted it"
    );

    let answer = app
        .post(
            path::<izlek_web::detail::DeleteTask>(),
            Some(&member),
            &[("task_id", &task)],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    // Gone from the board, and gone from the detail: soft is not visible.
    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(
        !answer.body.contains("Mistyped in a hurry"),
        "{}",
        answer.body
    );
    let answer = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
        .await;
    assert_eq!(answer.body, "{\"Err\":\"NotFound\"}");
}

/// Every column id on the board, in order, read off the wire the way the
/// browser would.
async fn columns_of(app: &App, cookie: &str) -> Vec<String> {
    let answer = app
        .post(path::<izlek_web::board::CurrentBoard>(), Some(cookie), &[])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    answer
        .body
        .split("{\"column\":{\"id\":\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn a_viewer_who_posts_a_move_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "quiet@izlek.sh", "Quiet Reader", Role::Viewer).await;
    let columns = columns_of(&app, &admin).await;
    let task = a_task(&app, &admin, &columns[0], "Stays in Backlog").await;

    let answer = app
        .post(
            path::<izlek_web::board::MoveCard>(),
            Some(&viewer),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "\"Forbidden\"");

    // And nothing moved: the refusal is in the handler, not in the drawing.
    let answer = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
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
    let columns = columns_of(&app, &admin).await;
    let task = a_task(&app, &admin, &columns[0], "Gets picked up").await;

    let answer = app
        .post(
            path::<izlek_web::board::MoveCard>(),
            Some(&member),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    let answer = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
        .await;
    assert!(answer.body.contains("\"moved\""), "no move in the activity");
}

#[tokio::test]
async fn a_drop_decided_against_a_stale_board_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@izlek.sh", "Mo Dubois", Role::Member).await;
    let columns = columns_of(&app, &admin).await;
    let task = a_task(&app, &admin, &columns[0], "Contested").await;

    // Two people picked the same card up out of Backlog.
    let first = app
        .post(
            path::<izlek_web::board::MoveCard>(),
            Some(&admin),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(first.body, "null");

    let second = app
        .post(
            path::<izlek_web::board::MoveCard>(),
            Some(&member),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[2]),
            ],
        )
        .await;
    assert_eq!(second.body, "\"MovedAlready\"");

    // The winner's move stands, and there is exactly one crossing.
    let answer = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
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
    let columns = columns_of(&app, &admin).await;
    let task = a_task(&app, &admin, &columns[0], "Stays put").await;

    let answer = app
        .post(
            path::<izlek_web::board::MoveCard>(),
            Some(&admin),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", "00000000-0000-0000-0000-000000000000"),
            ],
        )
        .await;
    assert_eq!(answer.body, "\"Forbidden\"");
}

// --- settings ---------------------------------------------------------------

#[tokio::test]
async fn a_signed_out_browser_is_told_nothing_by_the_settings_call() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);

    let answer = app
        .post(path::<izlek_web::settings::CurrentSettings>(), None, &[])
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "{\"Err\":\"SignInFirst\"}");
    assert!(!answer.body.contains("fastmail"), "{}", answer.body);
}

/// The sender panel is admin-only, and "only" is decided on the server: a
/// Member asking the same call gets an answer with no sender in it at all,
/// rather than a hidden panel whose contents rode along in the body.
#[tokio::test]
async fn a_member_is_told_nothing_about_the_sender() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&member),
            &[],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("\"administers\":false"), "{}", answer.body);
    assert!(answer.body.contains("\"sender\":null"), "{}", answer.body);
    assert!(!answer.body.contains("fastmail"), "{}", answer.body);
}

/// The admin sees the sender they typed — and never the password, which the
/// answer carries as a boolean and cannot carry as anything else.
#[tokio::test]
async fn an_admin_sees_the_sender_and_never_a_password() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);

    let answer = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(
        answer.body.contains(
            "\"sender\":{\"host\":\"smtp.fastmail.com\",\"port\":465,\
             \"username\":\"izlek\",\"from_name\":\"Izlek\",\
             \"from_address\":\"izlek@izlek.sh\",\"password_set\":true,\
             \"test\":null}"
        ),
        "{}",
        answer.body
    );
    assert!(
        !answer.body.contains(SENDER_PASSWORD),
        "the settings answer carried the SMTP password: {}",
        answer.body
    );
}

/// A Member cannot write the sender either. The panel they never received is a
/// courtesy; this is the guard, and it is in the handler.
#[tokio::test]
async fn only_an_admin_may_write_the_sender() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveSender>(),
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

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("Forbidden"), "{}", answer.body);

    // And nothing was written: the admin's own view still shows no sender.
    let seen = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(!seen.body.contains("attacker"), "{}", seen.body);
}

/// A signed-out browser gets the same refusal, and writes nothing.
#[tokio::test]
async fn a_signed_out_browser_may_not_write_the_sender() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveSender>(),
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

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("SignInFirst"), "{}", answer.body);

    let seen = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(!seen.body.contains("attacker"), "{}", seen.body);
}

/// The password field is write-only, so the form has nothing to send back for
/// a password that already exists. An edit with the field left empty must keep
/// the stored password rather than reading the blank as a deletion — otherwise
/// correcting a port silently stops the workspace sending mail.
#[tokio::test]
async fn an_edit_with_no_password_typed_keeps_the_stored_one() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    assert!(sender_saved(&app, &admin_cookie).await);

    let answer = app
        .post(
            path::<izlek_web::settings::SaveSender>(),
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
    assert_eq!(answer.body, "null", "{}", answer.body);

    let seen = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(seen.body.contains("\"port\":587"), "the edit did not land: {}", seen.body);
    assert!(
        seen.body.contains("\"password_set\":true"),
        "an empty field blanked the password: {}",
        seen.body
    );
}

/// The first save has to carry one, though. A sender with no password is a
/// sender that cannot sign in, and storing it would turn a form somebody can
/// fix into a queue of refusals they have to read the ledger to find.
#[tokio::test]
async fn a_first_sender_with_no_password_is_refused_and_says_why() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveSender>(),
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

    assert!(answer.body.contains("BadSender"), "{}", answer.body);
    assert!(answer.body.contains("password"), "{}", answer.body);
}

/// Each field the sender cannot work without is refused by name. A form that
/// answers "that did not work" sends somebody round the panel guessing.
#[tokio::test]
async fn a_sender_field_that_cannot_work_is_refused_by_name() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let bad: &[(&str, &[(&str, &str)])] = &[
        ("host", &[("host", "  ")]),
        ("host", &[("host", "smtp.fastmail.com/inbox")]),
        ("port", &[("port", "0")]),
        ("port", &[("port", "99999")]),
        ("username", &[("username", "")]),
        ("from_address", &[("from_address", "board-at-izlek")]),
        ("from_address", &[("from_address", "board@izlek")]),
    ];
    for (field, overrides) in bad {
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
        let answer = app
            .post(
                path::<izlek_web::settings::SaveSender>(),
                Some(&admin_cookie),
                &form,
            )
            .await;
        assert!(
            answer.body.contains("BadSender"),
            "{field} was accepted with {overrides:?}: {}",
            answer.body
        );
    }
}

/// The form carries a name and nothing else. Who is renamed comes from the
/// session, so there is no id to tamper with.
#[tokio::test]
async fn saving_a_profile_renames_the_person_asking_and_nobody_else() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveProfile>(),
            Some(&member),
            &[("display_name", "Emre Y")],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    let mine = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&member),
            &[],
        )
        .await;
    assert!(mine.body.contains("Emre Y"), "{}", mine.body);

    let theirs = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(theirs.body.contains("Ada Lovelace"), "{}", theirs.body);
}

#[tokio::test]
async fn a_profile_cannot_be_saved_without_a_name() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveProfile>(),
            Some(&admin_cookie),
            &[("display_name", "   ")],
        )
        .await;
    assert_eq!(answer.body, "\"EmptyName\"", "{}", answer.body);
}

#[tokio::test]
async fn a_signed_out_browser_cannot_rename_anybody() {
    let app = App::open().await;
    let _ = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveProfile>(),
            None,
            &[("display_name", "Whoever")],
        )
        .await;
    assert_eq!(answer.body, "\"SignInFirst\"", "{}", answer.body);
}

/// The limits are workspace content and the panel is admin-only. "Only" is
/// this handler, not the missing panel.
#[tokio::test]
async fn a_member_who_posts_new_limits_anyway_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveLimits>(),
            Some(&member),
            &[
                ("attachment_limit_mb", "400"),
                ("photo_limit_mb", "19"),
                ("allowed_file_types", "png"),
            ],
        )
        .await;
    assert_eq!(answer.body, "\"Forbidden\"", "{}", answer.body);

    // And the refusal is not cosmetic: the limits are where they were.
    let after = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(
        after.body.contains("\"attachment_limit_mb\":25"),
        "{}",
        after.body
    );
}

#[tokio::test]
async fn an_admin_changes_the_limits_and_they_stay_changed() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveLimits>(),
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "10"),
                ("photo_limit_mb", "1"),
                ("allowed_file_types", ".PNG, png, pdf"),
            ],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    let after = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(
        after.body.contains("\"attachment_limit_mb\":10"),
        "{}",
        after.body
    );
    assert!(after.body.contains("[\"png\",\"pdf\"]"), "{}", after.body);
}

/// A limit typed with an extra zero is a promise the disk cannot keep, and a
/// zero limit is an upload feature that refuses every file. Both are refused
/// by the handler rather than by the number input's max attribute.
#[tokio::test]
async fn a_limit_outside_what_the_disk_should_promise_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    for (attachment, photo) in [("5000", "2"), ("0", "2"), ("25", "0"), ("25", "200")] {
        let answer = app
            .post(
                path::<izlek_web::settings::SaveLimits>(),
                Some(&admin_cookie),
                &[
                    ("attachment_limit_mb", attachment),
                    ("photo_limit_mb", photo),
                    ("allowed_file_types", ""),
                ],
            )
            .await;
        assert_eq!(answer.body, "\"BadLimit\"", "{attachment}/{photo}");
    }
}

/// The allowed list is what an upload is checked against later, so a pattern
/// or a path cannot be stored in it as though it were an extension.
#[tokio::test]
async fn a_file_type_that_is_not_an_extension_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveLimits>(),
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "25"),
                ("photo_limit_mb", "2"),
                ("allowed_file_types", "../etc/passwd"),
            ],
        )
        .await;
    assert_eq!(answer.body, "\"BadFileType\"", "{}", answer.body);
}

/// The member list is the admin's. A Member asking gets an answer with no
/// list in it — not a table the page declined to draw.
#[tokio::test]
async fn a_member_is_not_sent_the_member_list() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let _ = invited(&app, &admin_cookie, "quiet@izlek.sh", "Quiet", Role::Viewer).await;

    let answer = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&member),
            &[],
        )
        .await;
    assert!(answer.body.contains("\"members\":null"), "{}", answer.body);
    assert!(!answer.body.contains("quiet@izlek.sh"), "{}", answer.body);
}

#[tokio::test]
async fn an_admin_sees_who_has_a_password_and_never_a_hash() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let _ = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    // Invited and never signed in: the account exists, the password does not.
    let answer = app
        .post(
            path::<izlek_web::auth::InviteMember>(),
            Some(&admin_cookie),
            &[
                ("email", "mert@izlek.sh"),
                ("display_name", "Mert"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);

    let answer = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(answer.body.contains("mert@izlek.sh"), "{}", answer.body);
    assert!(answer.body.contains("\"has_password\":false"), "{}", answer.body);
    assert!(answer.body.contains("\"has_password\":true"), "{}", answer.body);
    assert!(
        !answer.body.contains("$argon2"),
        "a hash reached the page: {}",
        answer.body
    );
}

/// Resending is admin-only, and "only" is the handler: a Member who posts it
/// gets no link back.
#[tokio::test]
async fn a_member_who_posts_a_resend_anyway_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let mert = app
        .post(
            path::<izlek_web::auth::InviteMember>(),
            Some(&admin_cookie),
            &[
                ("email", "mert@izlek.sh"),
                ("display_name", "Mert"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(mert.status, StatusCode::OK, "{}", mert.body);
    let members = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    let mert_id = members
        .body
        .split_once("\"id\":\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no member id")
        .to_string();

    let answer = app
        .post(
            path::<izlek_web::settings::ResendLink>(),
            Some(&member),
            &[("user_id", &mert_id)],
        )
        .await;
    assert_eq!(answer.body, "{\"Err\":\"Forbidden\"}", "{}", answer.body);
    assert!(!answer.body.contains("/join/"), "{}", answer.body);
}

/// An expired link is not a dead account: a resend opens the same one, and the
/// link it hands back is a working one.
#[tokio::test]
async fn a_resent_link_opens_the_same_account() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let invitation = app
        .post(
            path::<izlek_web::auth::InviteMember>(),
            Some(&admin_cookie),
            &[
                ("email", "mert@izlek.sh"),
                ("display_name", "Mert"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(invitation.status, StatusCode::OK, "{}", invitation.body);

    let list = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    let mert_id = list
        .body
        .split_once("mert@izlek.sh")
        .map(|(before, _)| before)
        .and_then(|before| before.rsplit_once("\"id\":\""))
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no member id")
        .to_string();

    let answer = app
        .post(
            path::<izlek_web::settings::ResendLink>(),
            Some(&admin_cookie),
            &[("user_id", &mert_id)],
        )
        .await;
    assert_eq!(answer.body, "{\"Ok\":\"mert@izlek.sh\"}", "{}", answer.body);
    let token = queued_join_token(&app, "mert@izlek.sh").await;

    let redeemed = app
        .post(
            path::<izlek_web::auth::RedeemLink>(),
            None,
            &[("token", &token), ("password", "lantern gravel spoon meadow")],
        )
        .await;
    assert_eq!(redeemed.body, "null", "{}", redeemed.body);
    assert!(redeemed.session.is_some(), "the resent link signed nobody in");
}

/// The first-sign-in screen greets the invited person with the name of whoever
/// made the account. It once read back their own name, which is both wrong and
/// the kind of wrong nobody reports.
#[tokio::test]
async fn an_invitation_names_the_admin_who_made_it_and_not_the_invitee() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::auth::InviteMember>(),
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
        .post(
            path::<izlek_web::auth::Invitation>(),
            None,
            &[("token", token.as_str())],
        )
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

/// Signing out ends the session on the server, not merely in the browser. A
/// cookie that survived in a copied header, a bookmarked tab or a shared
/// machine must stop working the moment the button is pressed.
#[tokio::test]
async fn signing_out_stops_the_session_being_worth_anything() {
    let app = App::open().await;
    let cookie = admin(&app).await;

    let works = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&cookie),
            &[],
        )
        .await;
    assert!(works.body.contains("administers"), "{}", works.body);

    let out = app
        .post(path::<izlek_web::auth::SignOut>(), Some(&cookie), &[])
        .await;
    assert_eq!(out.status, StatusCode::OK, "{}", out.body);

    // The same cookie, replayed. The server has to be the one refusing.
    let after = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&cookie),
            &[],
        )
        .await;
    assert!(
        !after.body.contains("administers"),
        "the session outlived signing out: {}",
        after.body
    );
}

/// A browser with no script is sent home, where no session means the sign-in
/// page. Without this the click looks like nothing happening on the very page
/// that is now about nobody.
#[tokio::test]
async fn signing_out_without_script_lands_on_the_sign_in_page() {
    let app = App::open().await;
    let cookie = admin(&app).await;

    let out = app
        .post_without_script(
            path::<izlek_web::auth::SignOut>(),
            Some(&cookie),
            "http://izlek.test/settings",
            &[],
        )
        .await;

    assert_eq!(out.status, StatusCode::FOUND, "{}", out.body);
    assert_eq!(out.location.as_deref(), Some("/"), "{:?}", out.location);
}

/// Signing out is not a way to sign anybody else out. The other person's
/// session is untouched, and so is every other browser of the person leaving.
#[tokio::test]
async fn signing_out_leaves_every_other_session_alone() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let out = app
        .post(path::<izlek_web::auth::SignOut>(), Some(&member), &[])
        .await;
    assert_eq!(out.status, StatusCode::OK, "{}", out.body);

    let still = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(still.body.contains("administers"), "{}", still.body);
}

#[tokio::test]
async fn a_member_may_not_press_the_test_button() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SendTestMail>(),
            Some(&member),
            &[],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("Forbidden"), "{}", answer.body);
}

#[tokio::test]
async fn a_viewer_may_not_press_the_test_button() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let viewer = invited(&app, &admin_cookie, "pinar@izlek.sh", "Pinar", Role::Viewer).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SendTestMail>(),
            Some(&viewer),
            &[],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("Forbidden"), "{}", answer.body);
}

#[tokio::test]
async fn a_signed_out_browser_may_not_press_the_test_button() {
    let app = App::open().await;

    let answer = app
        .post(path::<izlek_web::settings::SendTestMail>(), None, &[])
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("SignInFirst"), "{}", answer.body);
}

#[tokio::test]
async fn testing_a_sender_that_was_never_filled_in_says_so_rather_than_sending() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::settings::SendTestMail>(),
            Some(&admin_cookie),
            &[],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("BadSender"), "{}", answer.body);
    assert!(answer.body.contains("save it first"), "{}", answer.body);

    // Nothing was recorded either: the panel still has no test line to show.
    let seen = app
        .post(
            path::<izlek_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(!seen.body.contains("\"test\":{"), "{}", seen.body);
}

/// Writes one rule as the admin, and reports whether the server took it.
async fn rule_written(app: &App, admin: &str, column_id: &str, subject: &str) -> bool {
    let answer = app
        .post(
            path::<izlek_web::rules::CreateRule>(),
            Some(admin),
            &[
                ("trigger", "status"),
                ("column_id", column_id),
                ("subject", subject),
                ("audience", "assignees"),
            ],
        )
        .await;
    answer.status == StatusCode::OK && answer.body == "null"
}

/// The id of the one rule the workspace has.
async fn only_rule(app: &App, admin: &str) -> String {
    let answer = app
        .post(path::<izlek_web::rules::CurrentRules>(), Some(admin), &[])
        .await;
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
    let column = first_column(&app, &admin_cookie).await;

    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);

    let answer = app
        .post(
            path::<izlek_web::rules::CurrentRules>(),
            Some(&admin_cookie),
            &[],
        )
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
    assert!(answer.body.contains("\"last_sent\":null"), "{}", answer.body);
}

#[tokio::test]
async fn a_rule_with_no_subject_is_refused_and_says_why() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app, &admin_cookie).await;

    let answer = app
        .post(
            path::<izlek_web::rules::CreateRule>(),
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
async fn a_rule_may_not_be_hung_off_a_column_that_is_not_on_this_board() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<izlek_web::rules::CreateRule>(),
            Some(&admin_cookie),
            &[
                ("trigger", "status"),
                ("column_id", &Uuid::new_v4().to_string()),
                ("subject", "Task completed"),
                ("audience", "assignees"),
            ],
        )
        .await;
    assert!(answer.body.contains("Forbidden"), "{}", answer.body);

    let seen = app
        .post(
            path::<izlek_web::rules::CurrentRules>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(seen.body.contains("\"rules\":[]"), "{}", seen.body);
}

#[tokio::test]
async fn an_audience_the_screen_never_offers_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app, &admin_cookie).await;

    let answer = app
        .post(
            path::<izlek_web::rules::CreateRule>(),
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
    let column = first_column(&app, &admin_cookie).await;
    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);
    let rule = only_rule(&app, &admin_cookie).await;

    let answer = app
        .post(
            path::<izlek_web::rules::SetRuleEnabled>(),
            Some(&admin_cookie),
            &[("rule_id", &rule), ("enabled", "false")],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    // The screen lists what exists, not what is live.
    let seen = app
        .post(
            path::<izlek_web::rules::CurrentRules>(),
            Some(&admin_cookie),
            &[],
        )
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
    let stranger = Uuid::new_v4().to_string();

    for path in [
        path::<izlek_web::rules::SetRuleEnabled>(),
        path::<izlek_web::rules::DeleteRule>(),
    ] {
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
    let column = first_column(&app, &admin_cookie).await;
    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);
    let rule = only_rule(&app, &admin_cookie).await;

    let answer = app
        .post(
            path::<izlek_web::rules::DeleteRule>(),
            Some(&admin_cookie),
            &[("rule_id", &rule)],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    let seen = app
        .post(
            path::<izlek_web::rules::CurrentRules>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(seen.body.contains("\"rules\":[]"), "{}", seen.body);
}

#[tokio::test]
async fn only_an_admin_may_read_or_write_the_rules() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app, &admin_cookie).await;
    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);
    let rule = only_rule(&app, &admin_cookie).await;

    let member = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let viewer = invited(&app, &admin_cookie, "pinar@izlek.sh", "Pinar", Role::Viewer).await;

    for who in [&member, &viewer] {
        let read = app
            .post(path::<izlek_web::rules::CurrentRules>(), Some(who), &[])
            .await;
        assert!(read.body.contains("Forbidden"), "{}", read.body);

        let written = app
            .post(
                path::<izlek_web::rules::CreateRule>(),
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
                path::<izlek_web::rules::SetRuleEnabled>(),
                Some(who),
                &[("rule_id", &rule), ("enabled", "false")],
            )
            .await;
        assert!(switched.body.contains("Forbidden"), "{}", switched.body);

        let deleted = app
            .post(
                path::<izlek_web::rules::DeleteRule>(),
                Some(who),
                &[("rule_id", &rule)],
            )
            .await;
        assert!(deleted.body.contains("Forbidden"), "{}", deleted.body);
    }

    // And the rule is untouched: still there, still on.
    let seen = app
        .post(
            path::<izlek_web::rules::CurrentRules>(),
            Some(&admin_cookie),
            &[],
        )
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
    let column = first_column(&app, &admin_cookie).await;
    assert!(rule_written(&app, &admin_cookie, &column, "Task completed").await);
    let rule = only_rule(&app, &admin_cookie).await;

    for (path, form) in [
        (
            path::<izlek_web::rules::CurrentRules>(),
            vec![] as Vec<(&str, &str)>,
        ),
        (
            path::<izlek_web::rules::CreateRule>(),
            vec![
                ("trigger", "status"),
                ("column_id", column.as_str()),
                ("subject", "Task completed"),
                ("audience", "assignees"),
            ],
        ),
        (
            path::<izlek_web::rules::SetRuleEnabled>(),
            vec![("rule_id", rule.as_str()), ("enabled", "false")],
        ),
        (
            path::<izlek_web::rules::DeleteRule>(),
            vec![("rule_id", rule.as_str())],
        ),
    ] {
        let answer = app.post(path, None, &form).await;
        assert!(
            answer.body.contains("SignInFirst"),
            "{path}: {}",
            answer.body
        );
    }
}

// An id nobody's workspace owns answers `404` for a signed-in person, the
// same not-found a stranger to the task would see, and a `303` for nobody at
// all — never a leptos catch-all page, and never a `403` that would confirm
// the id belongs to someone else's workspace.
#[tokio::test]
async fn a_download_of_an_unknown_attachment_is_not_found_and_signed_out_is_a_redirect() {
    let app = App::open().await;
    let admin = admin(&app).await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/anything")
        .header(
            header::COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE}={admin}")).unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let request = Request::builder()
        .method("GET")
        .uri("/files/anything")
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

// A signed-in member's detail page carries the upload form no-script needs:
// a real multipart `<form>` posting to `/files`, not the leptos action form
// the rest of the screen uses — a browser with no script still has a way to
// attach a file.
#[tokio::test]
async fn the_files_section_is_on_the_detail_page() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Attach something to me").await;

    let request = Request::builder()
        .method("GET")
        .uri(format!("/?task={task}"))
        .header(
            header::COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE}={admin}")).unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        body.contains("multipart/form-data"),
        "no multipart upload form on the detail page"
    );
    assert!(
        body.contains(r#"action="/files""#),
        "the upload form does not post to /files"
    );
}

/// The id of the file named `name` in a [`izlek_web::detail::FetchTask`]
/// snapshot's body, read the way the page would: found by its name, then the
/// `id` field that comes right before it on the wire.
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
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "quiet@izlek.sh", "Quiet Reader", Role::Viewer).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Viewers cannot attach").await;

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
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@izlek.sh", "Mo Dubois", Role::Member).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Attach the spec").await;

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
        Some(format!("/?task={task}").as_str())
    );

    let snapshot = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&member),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "spec.png");

    let page = app.get(&format!("/?task={task}"), Some(&member)).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        html.contains(&format!("/files/{file_id}")),
        "no href to the new file: {html}"
    );
    assert!(html.contains("spec.png"));

    let download = app.get(&format!("/files/{file_id}"), Some(&member)).await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.bytes, png);
}

#[tokio::test]
async fn a_file_past_the_workspace_limit_is_refused_before_it_is_kept() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app, &admin_cookie).await;
    let task = a_task(&app, &admin_cookie, &column, "Too big to keep").await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveLimits>(),
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "1"),
                ("photo_limit_mb", "2"),
                ("allowed_file_types", ""),
            ],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

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
        Some(format!("/?task={task}&refusal=file-too-big&on=upload_file").as_str())
    );

    let snapshot = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
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
    let column = first_column(&app, &admin_cookie).await;
    let task = a_task(&app, &admin_cookie, &column, "No executables here").await;

    let answer = app
        .post(
            path::<izlek_web::settings::SaveLimits>(),
            Some(&admin_cookie),
            &[
                ("attachment_limit_mb", "25"),
                ("photo_limit_mb", "2"),
                ("allowed_file_types", "png"),
            ],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

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
        Some(format!("/?task={task}&refusal=file-type&on=upload_file").as_str())
    );
}

#[tokio::test]
async fn an_empty_allowed_list_lets_anything_through() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app, &admin_cookie).await;
    let task = a_task(&app, &admin_cookie, &column, "Whatever shows up").await;

    let answer = app
        .post_multipart(
            "/files",
            Some(&admin_cookie),
            &[("task_id", &task)],
            Some(("anything.bin", "application/octet-stream", &[0x00, 0x01, 0x02])),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some(format!("/?task={task}").as_str())
    );
}

#[tokio::test]
async fn the_stored_type_is_what_the_bytes_are_not_what_the_upload_claimed() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app, &admin_cookie).await;
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
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "liar.pdf");

    let download = app.get(&format!("/files/{file_id}"), Some(&admin_cookie)).await;
    assert_eq!(download.content_type.as_deref(), Some("image/png"));
}

#[tokio::test]
async fn a_file_name_that_is_a_path_is_kept_as_a_label() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app, &admin_cookie).await;
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
            path::<izlek_web::detail::FetchTask>(),
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

    let page = app.get(&format!("/?task={task}"), Some(&admin_cookie)).await;
    let html = String::from_utf8_lossy(&page.bytes);
    assert!(
        !html.contains("../../etc/passwd"),
        "the raw path leaked onto the chip: {html}"
    );

    let download = app.get(&format!("/files/{file_id}"), Some(&admin_cookie)).await;
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
    let column_b = first_column(&app_b, &admin_b).await;
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
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin_b),
            &[("task_id", &task_b)],
        )
        .await;
    let file_id = attachment_id_named(&snapshot.body, "theirs.png");

    let answer = app_a.get(&format!("/files/{file_id}"), Some(&admin_a)).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_upload_without_a_file_is_refused() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let column = first_column(&app, &admin_cookie).await;
    let task = a_task(&app, &admin_cookie, &column, "Nothing was chosen").await;

    let answer = app
        .post_multipart("/files", Some(&admin_cookie), &[("task_id", &task)], None)
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some(format!("/?task={task}&refusal=no-file&on=upload_file").as_str())
    );
}

#[tokio::test]
async fn only_the_uploader_or_an_admin_may_delete_a_file() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member_a = invited(&app, &admin_cookie, "asha@izlek.sh", "Asha", Role::Member).await;
    let member_b = invited(&app, &admin_cookie, "beau@izlek.sh", "Beau", Role::Member).await;
    let column = first_column(&app, &admin_cookie).await;
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
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    let file_a = attachment_id_named(&snapshot.body, "mine.png");
    let file_b = attachment_id_named(&snapshot.body, "theirs.png");

    let answer = app
        .post(
            path::<izlek_web::detail::DeleteFile>(),
            Some(&member_b),
            &[("file_id", &file_a)],
        )
        .await;
    assert_eq!(answer.body, "\"Forbidden\"", "{}", answer.body);

    let answer = app
        .post(
            path::<izlek_web::detail::DeleteFile>(),
            Some(&member_a),
            &[("file_id", &file_a)],
        )
        .await;
    assert_eq!(answer.body, "null", "the uploader was refused: {}", answer.body);

    let answer = app
        .post(
            path::<izlek_web::detail::DeleteFile>(),
            Some(&admin_cookie),
            &[("file_id", &file_b)],
        )
        .await;
    assert_eq!(answer.body, "null", "the admin was refused: {}", answer.body);

    let snapshot = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(&admin_cookie),
            &[("task_id", &task)],
        )
        .await;
    assert!(snapshot.body.contains("\"files\":[]"), "{}", snapshot.body);
}

/// The id of the assignable person on a task whose display name matches.
async fn person_id(app: &App, cookie: &str, task_id: &str, name: &str) -> String {
    let answer = app
        .post(
            path::<izlek_web::detail::FetchTask>(),
            Some(cookie),
            &[("task_id", task_id)],
        )
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
        let answer = app
            .post(path::<izlek_web::logs::CurrentLogs>(), Some(admin), &[])
            .await;
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
        let read = app
            .post(path::<izlek_web::logs::CurrentLogs>(), Some(who), &[])
            .await;
        assert!(read.body.contains("Forbidden"), "{}", read.body);
    }

    let out = app
        .post(path::<izlek_web::logs::CurrentLogs>(), None, &[])
        .await;
    assert!(out.body.contains("SignInFirst"), "{}", out.body);
}

#[tokio::test]
async fn an_admin_reads_the_logs() {
    let app = App::open_with_mail().await;
    let admin_cookie = admin(&app).await;
    let mate = invited(&app, &admin_cookie, "emre@izlek.sh", "Emre", Role::Member).await;
    let columns = columns_of(&app, &admin_cookie).await;
    let task = a_task(&app, &admin_cookie, &columns[0], "Ship it").await;
    let mate_id = person_id(&app, &admin_cookie, &task, "Emre").await;

    let assigned = app
        .post(
            path::<izlek_web::detail::Assign>(),
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
            path::<izlek_web::board::MoveCard>(),
            Some(&mate),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(moved.body, "null", "{}", moved.body);

    let snapshot = until_logs_contains(&app, &admin_cookie, "\"outcome\":\"nobody to mail\"").await;
    // The queue still carries Emre's invite mail — unrelated to this rule —
    // so the check is that the rule itself queued nothing, not an empty queue.
    assert!(!snapshot.contains("\"subject\":\"Task completed\""), "{}", snapshot);

    // The admin drops it back and moves it again: this time the mover is not
    // the assignee, so the rule owes Emre a mail. With no sender configured
    // the send is not a failure — it waits in the queue.
    let back = app
        .post(
            path::<izlek_web::board::MoveCard>(),
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[1]),
                ("to_column_id", &columns[0]),
            ],
        )
        .await;
    assert_eq!(back.body, "null", "{}", back.body);
    let forward = app
        .post(
            path::<izlek_web::board::MoveCard>(),
            Some(&admin_cookie),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(forward.body, "null", "{}", forward.body);

    // No sender means the send is held, not sent — the ledger stores that as
    // a failure with nothing spent, and the queue names the truth: held.
    let snapshot = until_logs_contains(&app, &admin_cookie, "\"recipient\":\"emre@izlek.sh\"").await;
    assert!(snapshot.contains("\"state\":\"held\""), "{}", snapshot);
    assert!(snapshot.contains("\"attempts\":0"), "{}", snapshot);
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
async fn until_rule_send_to(
    app: &App,
    rule_id: &str,
    recipient: &str,
    already: usize,
) -> izlek_core::store::MailSend {
    for _ in 0..500 {
        let matching: Vec<_> = app
            .store
            .mail_queue(50)
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
    let column = first_column(&app, &admin_cookie).await;
    let task = a_task(&app, &admin_cookie, &column, "Ship the picker").await;

    let created = app
        .post(
            path::<izlek_web::rules::CreateRule>(),
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
            path::<izlek_web::detail::PostComment>(),
            Some(&member),
            &[("task_id", &task), ("body", "Looks good")],
        )
        .await;
    assert_eq!(commented.body, "null", "{}", commented.body);

    let send = until_rule_send_to(&app, &rule, "ada@izlek.sh", 0).await;
    assert_eq!(send.recipient, "ada@izlek.sh");
    assert!(
        app.store.mail_queue(50).await.unwrap().iter().all(|send| {
            !(send.kind == SendKind::Rule
                && send.rule_id.as_deref() == Some(rule.as_str())
                && send.recipient == "deniz@izlek.sh")
        }),
        "the commenter was mailed instead of the creator"
    );
    let decisions = app.store.recent_mail_decisions(50).await.unwrap();
    assert!(
        decisions.iter().any(|decision| {
            decision.rule_id == rule
                && matches!(decision.outcome, izlek_core::store::MailOutcome::Owed)
        }),
        "the decisions ledger has no matched decision for the rule"
    );

    // The admin is put on the task so the rewritten rule's assignees audience
    // has someone to address once it fires on a rename.
    let admin_id = person_id(&app, &admin_cookie, &task, "Ada Lovelace").await;
    let assigned = app
        .post(
            path::<izlek_web::detail::Assign>(),
            Some(&admin_cookie),
            &[("task_id", &task), ("user_id", &admin_id)],
        )
        .await;
    assert_eq!(assigned.body, "null", "{}", assigned.body);

    // The rule is rewritten in place: same id, new trigger, subject and
    // audience.
    let updated = app
        .post(
            path::<izlek_web::rules::UpdateRule>(),
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
    assert_eq!(updated.body, "null", "the rewrite was refused: {}", updated.body);
    assert_eq!(
        only_rule(&app, &admin_cookie).await,
        rule,
        "a new rule was made instead of the old one rewritten"
    );

    let seen = app
        .post(path::<izlek_web::rules::CurrentRules>(), Some(&admin_cookie), &[])
        .await;
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
        .filter(|send| {
            send.kind == SendKind::Rule
                && send.rule_id.as_deref() == Some(rule.as_str())
                && send.recipient == "ada@izlek.sh"
        })
        .count();
    let renamed = app
        .post(
            path::<izlek_web::detail::SaveTask>(),
            Some(&member),
            &[("task_id", &task), ("title", "Ship the redesigned picker")],
        )
        .await;
    assert_eq!(renamed.body, "null", "{}", renamed.body);

    until_rule_send_to(&app, &rule, "ada@izlek.sh", already).await;
    assert!(
        app.store.mail_queue(50).await.unwrap().iter().all(|send| {
            !(send.kind == SendKind::Rule
                && send.rule_id.as_deref() == Some(rule.as_str())
                && send.recipient == "deniz@izlek.sh")
        }),
        "the renamer was mailed instead of being excluded as the actor"
    );

    let logs = until_logs_contains(&app, &admin_cookie, "\"subject\":\"Renamed\"").await;
    assert!(logs.contains("\"recipient\":\"ada@izlek.sh\""), "{}", logs);
}
